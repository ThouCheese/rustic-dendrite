use super::event_query::query_events_from_client;
use super::handler_registry::{HandlerRegistry, SubscriptionHandle, TheHandlerRegistry};
use super::{axon_serialize, ApplicableTo, AxonServerHandle, VecU8Message};
use crate::axon_server::command::command_provider_outbound;
use crate::axon_server::command::command_service_client::CommandServiceClient;
use crate::axon_server::command::{command_provider_inbound, Command};
use crate::axon_server::command::{CommandProviderOutbound, CommandResponse, CommandSubscription};
use crate::axon_server::event::event_store_client::EventStoreClient;
use crate::axon_server::event::Event;
use crate::axon_server::{ErrorMessage, FlowControl, SerializedObject};
use crate::intellij_work_around::Debuggable;
use anyhow::{anyhow, Result};
use async_stream::stream;
use futures_core::stream::Stream;
use log::{debug, error, warn};
use lru::LruCache;
use prost::Message;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tonic::transport::Channel;
use tonic::Request;
use uuid::Uuid;

/// Creates a struct that can be returned by a command handler to supply the events that have
/// to be emitted.
pub fn emit_events() -> EmitApplicableEventsAndResponse<()> {
    EmitApplicableEventsAndResponse {
        events: Vec::new(),
        response: None,
    }
}

/// Creates a struct that can be returned by a command handler to supply both the events that have
/// to be emitted and the response to the caller.
pub fn emit_events_and_response<T: Message, P: VecU8Message + Send + Clone>(
    type_name: &str,
    response: &T,
) -> Result<EmitApplicableEventsAndResponse<P>> {
    let payload = axon_serialize(type_name, response)?;
    Ok(EmitApplicableEventsAndResponse {
        events: Vec::new(),
        response: Some(payload),
    })
}

#[tonic::async_trait]
pub trait AggregateContextTrait<P: VecU8Message + Send + Sync + Clone + 'static> {
    fn emit(&mut self, event_type: &str, event: Box<dyn ApplicableTo<P>>) -> Result<()>;
    async fn get_projection(&mut self, aggregate_id: &str) -> Result<P>;
}

#[derive(Debug)]
pub struct AggregateContext<P: VecU8Message + Send + Sync + Clone + 'static> {
    aggregate_definition: Arc<AggregateDefinition<P>>,
    event_store_client: EventStoreClient<Channel>,
    events: Vec<(String, Box<dyn ApplicableTo<P>>)>,
    pub aggregate_id: Option<String>,
    projection: P,
    seq: i64,
}

#[tonic::async_trait]
impl<P: VecU8Message + Send + Sync + Clone + Debug> AggregateContextTrait<P>
    for AggregateContext<P>
{
    fn emit(&mut self, event_type: &str, event: Box<dyn ApplicableTo<P>>) -> Result<()> {
        self.events.push((event_type.to_string(), event));
        Ok(())
    }
    async fn get_projection(&mut self, aggregate_id: &str) -> Result<P> {
        let client = &mut self.event_store_client.clone();
        if let Some(ref existing_aggregate_id) = self.aggregate_id {
            if aggregate_id != existing_aggregate_id {
                anyhow!(
                    "Inconsistent aggregate_id: {:?}: {:?}",
                    aggregate_id,
                    existing_aggregate_id
                );
            }
        } else {
            self.aggregate_id = Some(aggregate_id.to_string());
        }
        let aggregate_definition = self.aggregate_definition.deref();
        {
            let mut cache = aggregate_definition
                .cache
                .lock()
                .map_err(|e| anyhow!(e.to_string()))?;
            if let Some((s, p)) = cache.get(&aggregate_id.to_string()) {
                debug!("Cache hit: {:?}: {:?}", aggregate_id, s);
                self.projection = p.clone();
                self.seq = *s;
            } else {
                debug!("Cache miss: {:?}", aggregate_id);
            }
        }
        if self.seq < 0 {
            let events = query_events_from_client(client, &aggregate_id).await?;
            for event in events {
                debug!("Replaying event: {:?}", Debuggable::from(&event));
                if let Some(payload) = event.payload {
                    let sourcing_handler = self
                        .aggregate_definition
                        .sourcing_handler_registry
                        .get(&payload.r#type)
                        .ok_or(anyhow!("Missing sourcing handler for {:?}", payload.r#type))?;
                    if let Some(p) = (sourcing_handler)
                        .handle(payload.data, self.projection.clone())
                        .await?
                    {
                        self.projection = p;
                    }
                }
                self.seq = event.aggregate_sequence_number;
            }
            debug!(
                "Restored projection: {:?}: {:?}",
                self.seq, &self.projection
            );
            if self.seq >= 0 {
                let mut cache = aggregate_definition
                    .cache
                    .lock()
                    .map_err(|e| anyhow!(e.to_string()))?;
                cache.put(
                    aggregate_id.to_string(),
                    (self.seq, self.projection.clone()),
                );
            }
        }
        Ok(self.projection.clone())
    }
}

impl<P: VecU8Message + Send + Sync + Clone> Clone for AggregateContext<P> {
    fn clone(&self) -> Self {
        let mut cloned_events = Vec::new();
        for (event_type, event) in &self.events {
            let cloned_pair = (event_type.clone(), event.box_clone());
            cloned_events.push(cloned_pair);
        }
        AggregateContext {
            aggregate_definition: self.aggregate_definition.clone(),
            event_store_client: self.event_store_client.clone(),
            events: cloned_events,
            aggregate_id: self.aggregate_id.clone(),
            projection: self.projection.clone(),
            seq: self.seq,
        }
    }
}

/// Struct that can be returned by a command handler to supply both the events that have
/// to be emitted and the response to the caller.
#[derive(Clone, Debug)]
pub struct EmitEventsAndResponse {
    events: Vec<SerializedObject>,
    response: Option<SerializedObject>,
}

/// Struct that can be returned by a command handler to supply both the events that have
/// to be emitted and the response to the caller.
///
/// The events have to be applicable to the projection type.
#[derive(Debug)]
pub struct EmitApplicableEventsAndResponse<P> {
    events: Vec<(String, Box<dyn ApplicableTo<P>>)>,
    response: Option<SerializedObject>,
}

impl<P> Clone for EmitApplicableEventsAndResponse<P> {
    fn clone(&self) -> Self {
        EmitApplicableEventsAndResponse {
            events: self
                .events
                .iter()
                .map(|(n, b)| (n.clone(), b.box_clone()))
                .collect(),
            response: self.response.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.events = source
            .events
            .iter()
            .map(|(n, b)| (n.clone(), b.box_clone()))
            .collect();
        self.response = source.response.clone();
    }
}

/// Trait that needs to be implemented by the aggregate registry.
pub trait AggregateRegistry {
    fn register(&mut self, applicator: &'static (dyn Fn(&mut Self) -> Result<()>)) -> Result<()>;
    fn insert(&mut self, aggregate_handle: Arc<dyn AggregateHandle>) -> Result<()>;
    fn get(&self, name: &str) -> Option<Arc<dyn AggregateHandle>>;
    fn register_commands(
        &self,
        commands: &mut Vec<String>,
        command_to_aggregate_mapping: &mut HashMap<String, String>,
    );
}

/// Concrete struct that implements `AggregateRegistry`.
pub struct TheAggregateRegistry {
    pub handlers: HashMap<String, Arc<dyn AggregateHandle>>,
}

impl AggregateRegistry for TheAggregateRegistry {
    fn register(&mut self, applicator: &'static (dyn Fn(&mut Self) -> Result<()>)) -> Result<()> {
        applicator(self)
    }
    fn insert(&mut self, aggregate_handle: Arc<dyn AggregateHandle>) -> Result<()> {
        self.handlers
            .insert(aggregate_handle.name(), aggregate_handle);
        Ok(())
    }

    fn get(&self, name: &str) -> Option<Arc<dyn AggregateHandle>> {
        self.handlers.get(name).map(Arc::clone)
    }

    fn register_commands(
        &self,
        commands: &mut Vec<String>,
        command_to_aggregate_mapping: &mut HashMap<String, String>,
    ) {
        for (aggregate_name, aggregate_handle) in &self.handlers {
            let command_names = aggregate_handle.command_names();
            for command_name in command_names {
                commands.push(command_name.clone());
                command_to_aggregate_mapping
                    .insert(command_name.clone(), (*aggregate_name).clone());
            }
        }
    }
}

/// Creates an empty aggregate registry that can be populated with `AggregateHandle`s (most likely: `AggregateDefinition`s).
pub fn empty_aggregate_registry() -> TheAggregateRegistry {
    TheAggregateRegistry {
        handlers: HashMap::new(),
    }
}

#[tonic::async_trait]
pub trait AggregateHandle: Send + Sync {
    fn name(&self) -> String;
    async fn handle(
        &self,
        command: &Command,
        client: &mut EventStoreClient<Channel>,
    ) -> Result<Option<EmitEventsAndResponse>>;
    fn command_names(&self) -> Vec<String>;
}

#[tonic::async_trait]
impl<P: VecU8Message + Send + Sync + Clone + std::fmt::Debug + 'static> AggregateHandle
    for Arc<AggregateDefinition<P>>
{
    fn name(&self) -> String {
        self.projection_name.clone()
    }
    async fn handle(
        &self,
        command: &Command,
        client: &mut EventStoreClient<Channel>,
    ) -> Result<Option<EmitEventsAndResponse>> {
        handle_command(command, self.clone(), client).await
    }
    fn command_names(&self) -> Vec<String> {
        let mut result = Vec::new();
        for (command_name, _) in &self.command_handler_registry.handlers {
            result.push((*command_name).clone());
        }
        result
    }
}

/// The complete definition of an aggregate.
///
/// Fields:
/// * `projection_name`: The name of the aggregate type.
/// * `cache`: Caches command projections in memory.
/// * `empty_projection`: Factory method for an empty projection.
/// * `sourcing_handler_registry`: Registry that assigns a handler for each event that updates the projection.
pub struct AggregateDefinition<P: VecU8Message + Send + Sync + Clone + 'static> {
    pub projection_name: String,
    cache: Arc<Mutex<LruCache<String, (i64, P)>>>,
    empty_projection: ProjectionFactory<P>,
    command_handler_registry:
        TheHandlerRegistry<Arc<async_lock::Mutex<AggregateContext<P>>>, SerializedObject>,
    sourcing_handler_registry: TheHandlerRegistry<P, P>,
}

pub struct ProjectionFactory<P> {
    factory: Box<fn() -> P>,
}

impl<P: VecU8Message + Send + Sync + Clone + 'static> Debug for AggregateDefinition<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "[AggregateDefinition:{:?}]",
            self.projection_name
        ))
        .unwrap();
        Ok(())
    }
}

/// Creates a new aggregate definition as needed by function `command_worker`.
pub fn create_aggregate_definition<P: VecU8Message + Send + Sync + Clone>(
    projection_name: String,
    empty_projection: Box<fn() -> P>,
    command_handler_registry: TheHandlerRegistry<
        Arc<async_lock::Mutex<AggregateContext<P>>>,
        SerializedObject,
    >,
    sourcing_handler_registry: TheHandlerRegistry<P, P>,
) -> AggregateDefinition<P> {
    let cache = Arc::new(Mutex::new(LruCache::new(1024)));
    let empty_projection = ProjectionFactory {
        factory: empty_projection,
    };
    AggregateDefinition {
        cache,
        projection_name,
        empty_projection,
        command_handler_registry,
        sourcing_handler_registry,
    }
}

async fn handle_command<P: VecU8Message + Send + Sync + Clone + std::fmt::Debug + 'static>(
    command: &Command,
    aggregate_definition: Arc<AggregateDefinition<P>>,
    client: &mut EventStoreClient<Channel>,
) -> Result<Option<EmitEventsAndResponse>> {
    debug!("Incoming command: {:?}", Debuggable::from(command));

    if let Some(command_handler) = get_command_handler(command.name.clone(), &aggregate_definition)?
    {
        let data = command
            .payload
            .clone()
            .map(|p| p.data)
            .ok_or(anyhow!("No payload data for: {:?}", command.name))?;

        let (result, events, aggregate_id, seq) =
            internal_handle_command(&command_handler, data, aggregate_definition.clone(), client)
                .await?;

        if !events.is_empty() {
            let aggregate_name = aggregate_definition.projection_name.clone();
            let aggregate_id = aggregate_id.ok_or(anyhow!("Missing aggregate id"))?;
            store_events(client, &aggregate_name, &aggregate_id, &events, seq + 1).await?;
        }
        Ok(Some(EmitEventsAndResponse {
            events: vec![],
            response: result,
        }))
    } else {
        Err(anyhow!("Missing command handler: {:?}", command.name))
    }
}

async fn internal_handle_command<
    P: VecU8Message + Send + Sync + Clone + std::fmt::Debug + 'static,
>(
    command_handler: &Box<
        dyn SubscriptionHandle<Arc<async_lock::Mutex<AggregateContext<P>>>, SerializedObject>,
    >,
    data: Vec<u8>,
    aggregate_definition: Arc<AggregateDefinition<P>>,
    event_store_client: &mut EventStoreClient<Channel>,
) -> Result<(
    Option<SerializedObject>,
    Vec<(String, Box<dyn ApplicableTo<P>>)>,
    Option<String>,
    i64,
)> {
    let empty_projection: P = (aggregate_definition.empty_projection.factory)();
    let aggregate_context = Arc::new(async_lock::Mutex::new(AggregateContext {
        aggregate_definition,
        event_store_client: event_store_client.clone(),
        events: Vec::new(),
        aggregate_id: None,
        projection: empty_projection,
        seq: -1,
    }));
    let result = command_handler
        .handle(data, aggregate_context.clone())
        .await?;
    let aggregate_context = &mut aggregate_context.deref().lock().await;
    let last_stored_seq = aggregate_context.seq;
    let aggregate_id = aggregate_context.aggregate_id.as_ref().map(|id| id.clone());
    if let Some(ref aggregate_id) = aggregate_id {
        let mut next_seq: i64 = last_stored_seq + 1;
        for pair in clone_events(&mut aggregate_context.events) {
            let aggregate_name = aggregate_context
                .aggregate_definition
                .projection_name
                .clone();
            let event = encode_event(&pair, &aggregate_name, aggregate_id, next_seq)?;
            debug!("Replaying new event: {:?}", Debuggable::from(&event));
            if let Some(payload) = event.payload {
                let sourcing_handler = aggregate_context
                    .aggregate_definition
                    .sourcing_handler_registry
                    .get(&payload.r#type)
                    .ok_or(anyhow!("Missing sourcing handler for {:?}", payload.r#type))?;
                if let Some(p) = (sourcing_handler)
                    .handle(payload.data, aggregate_context.projection.clone())
                    .await?
                {
                    aggregate_context.projection = p;
                    aggregate_context.seq = next_seq;
                    next_seq = next_seq + 1;
                }
            }
        }
        if next_seq > last_stored_seq + 1 {
            let mut cache = aggregate_context
                .aggregate_definition
                .cache
                .lock()
                .map_err(|e| anyhow!(e.to_string()))?;
            cache.put(
                aggregate_id.to_string(),
                (aggregate_context.seq, aggregate_context.projection.clone()),
            );
        }
    }
    let mut cloned_events = Vec::new();
    for (event_type, event) in &aggregate_context.events {
        let cloned_pair = (event_type.clone(), event.box_clone());
        cloned_events.push(cloned_pair);
    }
    Ok((
        result,
        cloned_events,
        aggregate_context.aggregate_id.clone(),
        last_stored_seq,
    ))
}

fn clone_events<P>(
    events: &mut Vec<(String, Box<dyn ApplicableTo<P>>)>,
) -> Vec<(String, Box<dyn ApplicableTo<P>>)> {
    let mut cloned_events = Vec::new();
    for (event_type, event) in events {
        cloned_events.push((event_type.clone(), event.box_clone()));
    }
    cloned_events
}

fn get_command_handler<P: VecU8Message + Send + Sync + Clone + std::fmt::Debug + 'static>(
    command_name: String,
    aggregate_definition: &Arc<AggregateDefinition<P>>,
) -> Result<
    Option<
        &Box<dyn SubscriptionHandle<Arc<async_lock::Mutex<AggregateContext<P>>>, SerializedObject>>,
    >,
> {
    let handler = aggregate_definition
        .command_handler_registry
        .get(&command_name);
    Ok(handler)
}

/// Adds an event that can be applied to the command projection to be emitted to the result of a command handler.
pub fn emit<P: VecU8Message + Send + Clone>(
    holder: &mut EmitApplicableEventsAndResponse<P>,
    type_name: &str,
    event: Box<dyn ApplicableTo<P>>,
) -> Result<()> {
    holder.events.push((type_name.to_string(), event));
    Ok(())
}

#[derive(Debug)]
struct AxonCommandResult {
    message_identifier: String,
    result: Result<Option<EmitEventsAndResponse>>,
}

/// Subscribes  to commands, verifies them against the command projection and sends emitted events to AxonServer.
pub async fn command_worker(
    axon_server_handle: AxonServerHandle,
    aggregate_registry: &mut TheAggregateRegistry,
) -> Result<()> {
    debug!("Command worker: start");

    let mut client = CommandServiceClient::new(axon_server_handle.conn.clone());
    let mut event_store_client = EventStoreClient::new(axon_server_handle.conn.clone());

    let mut command_to_aggregate_mapping = HashMap::new();
    let mut command_vec: Vec<String> = vec![];
    aggregate_registry.register_commands(&mut command_vec, &mut command_to_aggregate_mapping);
    let command_box = Box::new(command_vec);

    let (tx, rx): (Sender<AxonCommandResult>, Receiver<AxonCommandResult>) = channel(10);

    let outbound = create_output_stream(axon_server_handle, command_box, rx);

    debug!("Command worker: calling open_stream");
    let response = client.open_stream(Request::new(outbound)).await?;
    debug!("Stream response: {:?}", response);

    let mut inbound = response.into_inner();
    loop {
        match inbound.message().await {
            Ok(Some(inbound)) => {
                debug!("Inbound message: {:?}", Debuggable::from(&inbound));
                if let Some(command_provider_inbound::Request::Command(command)) = inbound.request {
                    let command_name = command.name.clone();
                    let mut result = Err(anyhow!("Could not find aggregate handler"));
                    if let Some(aggregate_name) = command_to_aggregate_mapping.get(&command_name) {
                        if let Some(aggregate_definition) = aggregate_registry.get(aggregate_name) {
                            result = aggregate_definition
                                .handle(&command, &mut event_store_client)
                                .await
                        }
                    }

                    match result.as_ref() {
                        Err(e) => warn!("Error while handling command: {:?}", e),
                        Ok(result) => debug!("Result from command handler: {:?}", result),
                    }

                    let axon_command_result = AxonCommandResult {
                        message_identifier: command.message_identifier,
                        result,
                    };
                    tx.send(axon_command_result).await.unwrap();
                }
            }
            Ok(None) => {
                debug!("None incoming");
            }
            Err(e) => {
                error!("Error from AxonServer: {:?}", e);
                return Err(anyhow!(e.code()));
            }
        }
    }
}

fn create_output_stream(
    axon_server_handle: AxonServerHandle,
    command_box: Box<Vec<String>>,
    mut rx: Receiver<AxonCommandResult>,
) -> impl Stream<Item = CommandProviderOutbound> {
    stream! {
        debug!("Command worker: stream: start: {:?}", rx);
        let client_id = axon_server_handle.client_id.clone();
        let component_name = axon_server_handle.display_name.clone();
        for command_name in command_box.iter() {
            debug!("Command worker: stream: subscribe to command type: {:?}", command_name);
            let subscription_id = Uuid::new_v4();
            let subscription = CommandSubscription {
                message_id: format!("{}", subscription_id),
                command: command_name.to_string().clone(),
                client_id: client_id.clone(),
                component_name: component_name.clone(),
                load_factor: 100,
            };
            debug!("Subscribe command: Subscription: {:?}", subscription);
            let instruction_id = Uuid::new_v4();
            debug!("Subscribe command: Instruction ID: {:?}", instruction_id);
            let instruction = CommandProviderOutbound {
                instruction_id: format!("{}", instruction_id),
                request: Some(command_provider_outbound::Request::Subscribe(subscription)),
            };
            yield instruction.to_owned();
        }

        let permits_batch_size: i64 = 3;
        let mut permits = permits_batch_size * 2;
        debug!("Command worker: stream: send initial flow-control permits: amount: {:?}", permits);
        let flow_control = FlowControl {
            client_id: client_id.clone(),
            permits,
        };
        let instruction_id = Uuid::new_v4();
        let instruction = CommandProviderOutbound {
            instruction_id: format!("{}", instruction_id),
            request: Some(command_provider_outbound::Request::FlowControl(flow_control)),
        };
        yield instruction.to_owned();

        while let Some(axon_command_result) = rx.recv().await {
            debug!("Send command response: {:?}", axon_command_result);
            let response_id = Uuid::new_v4();
            let mut response = CommandResponse {
                message_identifier: format!("{}", response_id),
                request_identifier: axon_command_result.message_identifier.clone(),
                payload: None,
                error_code: "".to_string(),
                error_message: None,
                meta_data: HashMap::new(),
                processing_instructions: Vec::new(),
            };
            match axon_command_result.result {
                Ok(result) => {
                    response.payload = result.map(|r| r.response).flatten();
                }
                Err(e) => {
                    response.error_code = "ERROR".to_string();
                    response.error_message = Some(ErrorMessage {
                        message: e.to_string(),
                        location: "".to_string(),
                        details: Vec::new(),
                        error_code: "ERROR".to_string(),
                    });
                }
            }
            let instruction_id = Uuid::new_v4();
            let instruction = CommandProviderOutbound {
                instruction_id: format!("{}", instruction_id),
                request: Some(command_provider_outbound::Request::CommandResponse(response)),
            };
            yield instruction.to_owned();
            permits -= 1;
            if permits <= permits_batch_size {
                debug!("Command worker: stream: send more flow-control permits: amount: {:?}", permits_batch_size);
                let flow_control = FlowControl {
                    client_id: client_id.clone(),
                    permits: permits_batch_size,
                };
                let instruction_id = Uuid::new_v4();
                let instruction = CommandProviderOutbound {
                    instruction_id: format!("{}", instruction_id),
                    request: Some(command_provider_outbound::Request::FlowControl(flow_control)),
                };
                yield instruction.to_owned();
                permits += permits_batch_size;
            }
            debug!("Command worker: stream: flow-control permits: balance: {:?}", permits);
        }

        // debug!("Command worker: stream: stop");
    }
}

async fn store_events<P: std::fmt::Debug>(
    client: &mut EventStoreClient<Channel>,
    aggregate_name: &str,
    aggregate_id: &str,
    events: &Vec<(String, Box<dyn ApplicableTo<P>>)>,
    next_seq: i64,
) -> Result<()> {
    debug!("Store events: Client: {:?}: events: {:?}", client, events);

    let now = std::time::SystemTime::now();
    let timestamp = now.duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    let event_messages: Vec<Event> = events
        .iter()
        .map(move |e| {
            encode_event_with_timestamp(e, aggregate_name, aggregate_id, timestamp, next_seq)
        })
        .collect();
    let request = Request::new(futures_util::stream::iter(event_messages));
    client.append_event(request).await?;
    Ok(())
}

fn encode_event<P>(
    e: &(String, Box<dyn ApplicableTo<P>>),
    aggregate_name: &str,
    aggregate_id: &str,
    next_seq: i64,
) -> Result<Event> {
    let now = std::time::SystemTime::now();
    let timestamp = now.duration_since(std::time::UNIX_EPOCH)?.as_millis() as i64;
    Ok(encode_event_with_timestamp(
        e,
        aggregate_name,
        aggregate_id,
        timestamp,
        next_seq,
    ))
}

fn encode_event_with_timestamp<P>(
    e: &(String, Box<dyn ApplicableTo<P>>),
    aggregate_name: &str,
    aggregate_id: &str,
    timestamp: i64,
    next_seq: i64,
) -> Event {
    let (type_name, event) = e;
    let mut buf = Vec::new();
    event.encode_u8(&mut buf).unwrap();
    let e = SerializedObject {
        r#type: type_name.to_string(),
        revision: "".to_string(),
        data: buf,
    };
    let message_identifier = Uuid::new_v4();
    Event {
        message_identifier: format!("{}", message_identifier),
        timestamp,
        aggregate_identifier: aggregate_id.to_string(),
        aggregate_sequence_number: next_seq,
        aggregate_type: aggregate_name.to_string(),
        payload: Some(e),
        meta_data: HashMap::new(),
        snapshot: false,
    }
}
