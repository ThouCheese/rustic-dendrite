use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_core::Future;
use futures_util::__private::Pin;
use prost::DecodeError;
use std::collections::HashMap;

/// Describes a registry for handlers for a particular type projection (or context) and a particular return type.
// I tried to make it possible to pass an `async fn` directly to parameter `handler`, but the return
// type after desugaring is unnameable
// (https://internals.rust-lang.org/t/naming-the-return-type-of-an-async-function/10085).
// So it has to be wrapped in a closure that `Box::pin`s the return value, lik this:
//
// ```rust
//     handler_registry.insert(
//         "GreetCommand",
//         &GreetCommand::decode,
//         &(|c| Box::pin(handle_greet_command(c)))
//     )?;
// ```
//
// P: type of the projection that serves as context for the handlers
// W: type of the wrapped result
pub trait HandlerRegistry<P, W>: Send {
    fn register(&mut self, applicator: &'static (dyn Fn(&mut Self) -> Result<()>)) -> Result<()>;
    fn insert<T: Send + Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Sync),
    ) -> Result<()>;
    fn insert_ignoring_output<T: Send + Clone, R: Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<R>>> + Send>>
                      + Sync),
    ) -> Result<()>;
    fn insert_with_output<T: Send + Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<W>>> + Send>>
                      + Sync),
    ) -> Result<()>;
    fn insert_with_mapped_output<T: Send + Clone, R: Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<R>>> + Send>>
                      + Sync),
        type_name: &str,
        wrapper: &'static (dyn Fn(&str, &R) -> Result<W> + Sync),
    ) -> Result<()>;
    fn get(&self, name: &str) -> Option<&Box<dyn SubscriptionHandle<P, W>>>;
}

/// Concrete struct that implements the `HandlerRegistry` trait.
pub struct TheHandlerRegistry<P: Send, W: Clone> {
    pub handlers: HashMap<String, Box<dyn SubscriptionHandle<P, W>>>,
}

impl<P: Send + Clone, W: Clone + 'static> HandlerRegistry<P, W> for TheHandlerRegistry<P, W> {
    fn register(&mut self, applicator: &'static (dyn Fn(&mut Self) -> Result<()>)) -> Result<()> {
        applicator(self)
    }

    fn insert<T: Send + Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Sync),
    ) -> Result<()> {
        let name = name.to_string();
        let key = name.clone();
        let handle: Box<dyn SubscriptionHandle<P, W>> = Box::new(SubscriptionVoid {
            name,
            deserializer,
            handler,
        });
        if (*self).handlers.contains_key(&key) {
            return Err(anyhow!("Handler already registered: {:?}", key));
        }
        (*self).handlers.insert(key.clone(), handle.box_clone());
        Ok(())
    }

    fn insert_ignoring_output<T: Send + Clone, R: Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<R>>> + Send>>
                      + Sync),
    ) -> Result<()> {
        let name = name.to_string();
        let key = name.clone();
        let handle: Box<dyn SubscriptionHandle<P, W>> = Box::new(Subscription {
            name,
            deserializer,
            handler,
            wrapper: None,
        });
        if (*self).handlers.contains_key(&key) {
            return Err(anyhow!("Handler already registered: {:?}", key));
        }
        (*self).handlers.insert(key.clone(), handle.box_clone());
        Ok(())
    }

    fn insert_with_output<T: Send + Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<W>>> + Send>>
                      + Sync),
    ) -> Result<()> {
        let name = name.to_string();
        let key = name.clone();
        let handle: Box<dyn SubscriptionHandle<P, W>> = Box::new(Subscription {
            name,
            deserializer,
            handler,
            wrapper: Some(ResponseWrapper {
                type_name: "UNKNOWN".to_string(),
                convert: &(|_, r| Ok(r.clone())),
            }),
        });
        if (*self).handlers.contains_key(&key) {
            return Err(anyhow!("Handler already registered: {:?}", key));
        }
        (*self).handlers.insert(key.clone(), handle.box_clone());
        Ok(())
    }

    fn insert_with_mapped_output<T: Send + Clone, R: Clone>(
        &mut self,
        name: &str,
        deserializer: &'static (dyn Fn(Bytes) -> Result<T, DecodeError> + Sync),
        handler: &'static (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<R>>> + Send>>
                      + Sync),
        type_name: &str,
        wrapper: &'static (dyn Fn(&str, &R) -> Result<W> + Sync),
    ) -> Result<()> {
        let name = name.to_string();
        let key = name.clone();
        let handle: Box<dyn SubscriptionHandle<P, W>> = Box::new(Subscription {
            name,
            deserializer,
            handler,
            wrapper: Some(ResponseWrapper {
                type_name: type_name.to_string(),
                convert: wrapper,
            }),
        });
        if (*self).handlers.contains_key(&key) {
            return Err(anyhow!("Handler already registered: {:?}", key));
        }
        (*self).handlers.insert(key.clone(), handle.box_clone());
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&Box<dyn SubscriptionHandle<P, W>>> {
        (*self).handlers.get(name)
    }
}

/// Creates an empty handler registry for a type of projection and a type of return values that can be populated with SubscriptionHandles.
pub fn empty_handler_registry<P: Send, W: Clone>() -> TheHandlerRegistry<P, W> {
    TheHandlerRegistry {
        handlers: HashMap::new(),
    }
}

#[tonic::async_trait]
pub trait SubscriptionHandle<P, W>: Send + Sync {
    fn name(&self) -> String;
    async fn handle(&self, buf: Vec<u8>, projection: P) -> Result<Option<W>>;
    fn box_clone(&self) -> Box<dyn SubscriptionHandle<P, W>>;
}

#[derive(Clone)]
struct Subscription<'a, P, T, R, W> {
    pub name: String,
    pub deserializer: &'a (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
    pub handler:
        &'a (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<Option<R>>> + Send>> + Sync),
    pub wrapper: Option<ResponseWrapper<'a, R, W>>,
}

#[derive(Clone)]
struct ResponseWrapper<'a, R, W> {
    pub type_name: String,
    pub convert: &'a (dyn Fn(&str, &R) -> Result<W> + Sync),
}

#[tonic::async_trait]
impl<P: Send + Clone, T: Send + Clone, R: Clone, W: Clone> SubscriptionHandle<P, W>
    for Subscription<'static, P, T, R, W>
{
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn handle(&self, buf: Vec<u8>, projection: P) -> Result<Option<W>> {
        let message: T = (self.deserializer)(Bytes::from(buf))?;
        if let Some(result) = (self.handler)(message, projection).await? {
            if let Some(wrapper) = self.wrapper.as_ref() {
                return Ok(Some((wrapper.convert)(&wrapper.type_name, &result)?));
            }
        }
        Ok(None)
    }

    fn box_clone(&self) -> Box<dyn SubscriptionHandle<P, W>> {
        Box::from(Subscription::clone(&self))
    }
}

#[derive(Clone)]
struct SubscriptionVoid<'a, P, T> {
    pub name: String,
    pub deserializer: &'a (dyn Fn(Bytes) -> Result<T, prost::DecodeError> + Sync),
    pub handler: &'a (dyn Fn(T, P) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Sync),
}

#[tonic::async_trait]
impl<P: Send + Clone, T: Send + Clone, W: Clone + 'static> SubscriptionHandle<P, W>
    for SubscriptionVoid<'static, P, T>
{
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn handle(&self, buf: Vec<u8>, projection: P) -> Result<Option<W>> {
        let message: T = (self.deserializer)(Bytes::from(buf))?;
        (self.handler)(message, projection).await?;
        Ok(None)
    }

    fn box_clone(&self) -> Box<dyn SubscriptionHandle<P, W>> {
        Box::from(SubscriptionVoid::clone(&self))
    }
}
