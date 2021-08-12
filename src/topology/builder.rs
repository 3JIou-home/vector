use super::{
    fanout::{self, Fanout},
    task::{Task, TaskOutput},
    BuiltBuffer, ConfigDiff,
};
use crate::{
    buffers,
    config::{ComponentScope, DataType, ProxyConfig, SinkContext, SourceContext},
    event::Event,
    internal_events::{EventIn, EventOut, EventZeroIn},
    shutdown::SourceShutdownCoordinator,
    transforms::Transform,
    Pipeline,
};
use futures::{future, stream, FutureExt, SinkExt, StreamExt, TryFutureExt};
use std::pin::Pin;
use std::{
    collections::HashMap,
    future::ready,
    sync::{Arc, Mutex},
};
use stream_cancel::{StreamExt as StreamCancelExt, Trigger, Tripwire};
use tokio::time::{timeout, Duration};

pub struct Pieces {
    pub inputs: HashMap<ComponentScope, (buffers::BufferInputCloner<Event>, Vec<ComponentScope>)>,
    pub outputs: HashMap<ComponentScope, fanout::ControlChannel>,
    pub tasks: HashMap<ComponentScope, Task>,
    pub source_tasks: HashMap<ComponentScope, Task>,
    pub healthchecks: HashMap<ComponentScope, Task>,
    pub shutdown_coordinator: SourceShutdownCoordinator,
    pub detach_triggers: HashMap<ComponentScope, Trigger>,
}

/// Builds only the new pieces, and doesn't check their topology.
pub async fn build_pieces(
    config: &super::Config,
    diff: &ConfigDiff,
    mut buffers: HashMap<String, BuiltBuffer>,
) -> Result<Pieces, Vec<String>> {
    let mut inputs = HashMap::new();
    let mut outputs = HashMap::new();
    let mut tasks = HashMap::new();
    let mut source_tasks = HashMap::new();
    let mut healthchecks = HashMap::new();
    let mut shutdown_coordinator = SourceShutdownCoordinator::default();
    let mut detach_triggers = HashMap::new();

    let mut errors = vec![];

    // Build sources
    for (scope, source) in config
        .sources
        .iter()
        .filter(|(scope, _)| diff.sources.contains_new(*scope))
    {
        let (tx, rx) = futures::channel::mpsc::channel(1000);
        let pipeline = Pipeline::from_sender(tx, vec![]);

        let typetag = source.inner.source_type();

        let (shutdown_signal, force_shutdown_tripwire) =
            shutdown_coordinator.register_source(scope.name());

        let context = SourceContext {
            scope: scope.clone(),
            globals: config.global.clone(),
            shutdown: shutdown_signal,
            out: pipeline,
            acknowledgements: source.acknowledgements,
            proxy: ProxyConfig::merge_with_env(&config.global.proxy, &source.proxy),
        };
        let server = match source.inner.build(context).await {
            Err(error) => {
                errors.push(format!("Source {:?}: {}", scope, error));
                continue;
            }
            Ok(server) => server,
        };

        let (output, control) = Fanout::new();
        let pump = rx.map(Ok).forward(output).map_ok(|_| TaskOutput::Source);
        let pump = Task::new(scope.clone(), typetag, pump);

        // The force_shutdown_tripwire is a Future that when it resolves means that this source
        // has failed to shut down gracefully within its allotted time window and instead should be
        // forcibly shut down. We accomplish this by select()-ing on the server Task with the
        // force_shutdown_tripwire. That means that if the force_shutdown_tripwire resolves while
        // the server Task is still running the Task will simply be dropped on the floor.
        let server = async {
            emit!(EventZeroIn);
            match future::try_select(server, force_shutdown_tripwire.unit_error().boxed()).await {
                Ok(_) => {
                    debug!("Finished.");
                    Ok(TaskOutput::Source)
                }
                Err(_) => Err(()),
            }
        };
        let server = Task::new(scope.clone(), typetag, server);

        outputs.insert(scope.clone(), control);
        tasks.insert(scope.clone(), pump);
        source_tasks.insert(scope.clone(), server);
    }

    // Build transforms
    for (scope, transform) in config
        .transforms
        .iter()
        .filter(|(scope, _)| diff.transforms.contains_new(scope))
    {
        let trans_inputs = &transform.inputs;

        let typetag = transform.inner.transform_type();

        let input_type = transform.inner.input_type();
        let transform = match transform.inner.build(&config.global).await {
            Err(error) => {
                errors.push(format!("Transform {:?}: {}", scope, error));
                continue;
            }
            Ok(transform) => transform,
        };

        let (input_tx, input_rx, _) =
            vector_core::buffers::build(vector_core::buffers::Variant::Memory {
                max_events: 100,
                when_full: vector_core::buffers::WhenFull::Block,
            })
            .unwrap();
        let input_rx = crate::utilization::wrap(Pin::new(input_rx));

        let (output, control) = Fanout::new();

        let transform = match transform {
            Transform::Function(mut t) => input_rx
                .filter(move |event| ready(filter_event_type(event, input_type)))
                .inspect(|_| emit!(EventIn))
                .flat_map(move |v| {
                    let mut buf = Vec::with_capacity(1);
                    t.transform(&mut buf, v);
                    emit!(EventOut { count: buf.len() });
                    stream::iter(buf.into_iter()).map(Ok)
                })
                .forward(output)
                .boxed(),
            Transform::Task(t) => {
                let filtered = input_rx
                    .filter(move |event| ready(filter_event_type(event, input_type)))
                    .inspect(|_| emit!(EventIn));
                t.transform(Box::pin(filtered))
                    .map(Ok)
                    .forward(output.with(|event| async {
                        emit!(EventOut { count: 1 });
                        Ok(event)
                    }))
                    .boxed()
            }
        }
        .map_ok(|_| {
            debug!("Finished.");
            TaskOutput::Transform
        });
        let task = Task::new(scope.clone(), typetag, transform);

        inputs.insert(scope.clone(), (input_tx, trans_inputs.clone()));
        outputs.insert(scope.clone(), control);
        tasks.insert(scope.clone(), task);
    }

    // Build sinks
    for (scope, sink) in config
        .sinks
        .iter()
        .filter(|(scope, _)| diff.sinks.contains_new(scope))
    {
        let sink_inputs = &sink.inputs;
        let sink = &sink.inner;
        let healthcheck = sink.healthcheck();
        let enable_healthcheck = healthcheck.enabled && config.healthchecks.enabled;

        let typetag = sink.inner.sink_type();
        let input_type = sink.inner.input_type();

        let (tx, rx, acker) = if let Some(buffer) = buffers.remove(scope.name()) {
            buffer
        } else {
            let buffer = sink.buffer.build(&config.global.data_dir, scope.name());
            match buffer {
                Err(error) => {
                    errors.push(format!("Sink {:?}: {}", scope, error));
                    continue;
                }
                Ok((tx, rx, acker)) => (tx, Arc::new(Mutex::new(Some(rx.into()))), acker),
            }
        };

        let cx = SinkContext {
            acker: acker.clone(),
            healthcheck,
            globals: config.global.clone(),
            proxy: ProxyConfig::merge_with_env(&config.global.proxy, sink.proxy()),
        };

        let (sink, healthcheck) = match sink.inner.build(cx).await {
            Err(error) => {
                errors.push(format!("Sink {:?}: {}", scope, error));
                continue;
            }
            Ok(built) => built,
        };

        let (trigger, tripwire) = Tripwire::new();

        let sink = async move {
            // Why is this Arc<Mutex<Option<_>>> needed you ask.
            // In case when this function build_pieces errors
            // this future won't be run so this rx won't be taken
            // which will enable us to reuse rx to rebuild
            // old configuration by passing this Arc<Mutex<Option<_>>>
            // yet again.
            let rx = rx
                .lock()
                .unwrap()
                .take()
                .expect("Task started but input has been taken.");

            let mut rx = Box::pin(crate::utilization::wrap(rx));

            sink.run(
                rx.by_ref()
                    .filter(|event| ready(filter_event_type(event, input_type)))
                    .inspect(|_| emit!(EventIn))
                    .take_until_if(tripwire),
            )
            .await
            .map(|_| {
                debug!("Finished.");
                TaskOutput::Sink(rx, acker)
            })
        };
        let task = Task::new(scope.clone(), typetag, sink);

        let component_name = scope.name().to_string();
        let component_scope = scope.scope().cloned();
        let healthcheck_task = async move {
            if enable_healthcheck {
                let duration = Duration::from_secs(10);
                timeout(duration, healthcheck)
                    .map(|result| match result {
                        Ok(Ok(_)) => {
                            info!("Healthcheck: Passed.");
                            Ok(TaskOutput::Healthcheck)
                        }
                        Ok(Err(error)) => {
                            error!(
                                msg = "Healthcheck: Failed Reason.",
                                %error,
                                component_kind = "sink",
                                component_type = typetag,
                                ?component_name,
                                ?component_scope,
                            );
                            Err(())
                        }
                        Err(_) => {
                            error!(
                                msg = "Healthcheck: timeout.",
                                component_kind = "sink",
                                component_type = typetag,
                                ?component_name,
                                ?component_scope
                            );
                            Err(())
                        }
                    })
                    .await
            } else {
                info!("Healthcheck: Disabled.");
                Ok(TaskOutput::Healthcheck)
            }
        };
        let healthcheck_task = Task::new(scope.clone(), typetag, healthcheck_task);

        inputs.insert(scope.clone(), (tx, sink_inputs.clone()));
        healthchecks.insert(scope.clone(), healthcheck_task);
        tasks.insert(scope.clone(), task);
        detach_triggers.insert(scope.clone(), trigger);
    }

    if errors.is_empty() {
        let pieces = Pieces {
            inputs,
            outputs,
            tasks,
            source_tasks,
            healthchecks,
            shutdown_coordinator,
            detach_triggers,
        };

        Ok(pieces)
    } else {
        Err(errors)
    }
}

fn filter_event_type(event: &Event, data_type: DataType) -> bool {
    match data_type {
        DataType::Any => true,
        DataType::Log => matches!(event, Event::Log(_)),
        DataType::Metric => matches!(event, Event::Metric(_)),
    }
}
