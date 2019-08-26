use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use downcast::Downcast;

use wayland_commons::filter::Filter;
use wayland_commons::map::ObjectMap;
use wayland_commons::wire::Message;
use wayland_commons::MessageGroup;

use crate::{Interface, Main, Proxy};

mod connection;
mod display;
mod proxy;
mod queues;

pub(crate) use self::display::DisplayInner;
pub(crate) use self::proxy::ProxyInner;
pub(crate) use self::queues::EventQueueInner;

/// A handle to the object map internal to the library state
///
/// This type is only used by code generated by `wayland-scanner`, and can not
/// be instantiated directly.
pub struct ProxyMap {
    map: Arc<Mutex<ObjectMap<self::proxy::ObjectMeta>>>,
    connection: Arc<Mutex<self::connection::Connection>>,
}

impl ProxyMap {
    pub(crate) fn make(
        map: Arc<Mutex<ObjectMap<self::proxy::ObjectMeta>>>,
        connection: Arc<Mutex<self::connection::Connection>>,
    ) -> ProxyMap {
        ProxyMap { map, connection }
    }

    /// Returns the Proxy corresponding to a given id
    pub fn get<I: Interface + AsRef<Proxy<I>> + From<Proxy<I>>>(&mut self, id: u32) -> Option<Proxy<I>> {
        ProxyInner::from_id(id, self.map.clone(), self.connection.clone()).map(|object| {
            debug_assert!(I::NAME == "<anonymous>" || object.is_interface::<I>());
            Proxy::wrap(object)
        })
    }

    /// Creates a new proxy for given id
    pub fn get_new<I: Interface + AsRef<Proxy<I>> + From<Proxy<I>>>(&mut self, id: u32) -> Option<Main<I>> {
        debug_assert!(self
            .map
            .lock()
            .unwrap()
            .find(id)
            .map(|obj| obj.is_interface::<I>())
            .unwrap_or(true));
        ProxyInner::from_id(id, self.map.clone(), self.connection.clone()).map(Main::wrap)
    }
}

/// Stores a value in a threadafe container that
/// only lets you access it from its owning thread
struct ThreadGuard<T> {
    val: T,
    thread: std::thread::ThreadId,
}

impl<T> ThreadGuard<T> {
    pub fn new(val: T) -> ThreadGuard<T> {
        ThreadGuard {
            val,
            thread: std::thread::current().id(),
        }
    }

    pub fn get(&self) -> &T {
        assert!(
            self.thread == std::thread::current().id(),
            "Attempted to access a ThreadGuard contents from the wrong thread."
        );
        &self.val
    }
}

unsafe impl<T> Send for ThreadGuard<T> {}
unsafe impl<T> Sync for ThreadGuard<T> {}

/*
 * Dispatching logic
 */
pub(crate) enum Dispatched {
    Yes,
    NoDispatch(Message, ProxyInner),
    BadMsg,
}

pub(crate) trait Dispatcher: Downcast + Send {
    fn dispatch(&mut self, msg: Message, proxy: ProxyInner, map: &mut ProxyMap) -> Dispatched;
}

mod dispatcher_impl {
    // this mod has the sole purpose of silencing these `dead_code` warnings...
    #![allow(dead_code)]
    use super::Dispatcher;
    impl_downcast!(Dispatcher);
}

pub(crate) struct ImplDispatcher<
    I: Interface + AsRef<Proxy<I>> + From<Proxy<I>>,
    F: FnMut(I::Event, Main<I>) + 'static,
> {
    _i: ::std::marker::PhantomData<&'static I>,
    implementation: F,
}

impl<I, F> Dispatcher for ImplDispatcher<I, F>
where
    I: Interface + AsRef<Proxy<I>> + From<Proxy<I>> + Sync,
    F: FnMut(I::Event, Main<I>) + 'static + Send,
    I::Event: MessageGroup<Map = ProxyMap>,
{
    fn dispatch(&mut self, msg: Message, proxy: ProxyInner, map: &mut ProxyMap) -> Dispatched {
        let opcode = msg.opcode as usize;
        if ::std::env::var_os("WAYLAND_DEBUG").is_some() {
            eprintln!(
                " <- {}@{}: {} {:?}",
                proxy.object.interface, proxy.id, proxy.object.events[opcode].name, msg.args
            );
        }
        let message = match I::Event::from_raw(msg, map) {
            Ok(v) => v,
            Err(()) => return Dispatched::BadMsg,
        };
        if message.since() > proxy.version() {
            eprintln!(
                "Received an event {} requiring version >= {} while proxy {}@{} is version {}.",
                proxy.object.events[opcode].name,
                message.since(),
                proxy.object.interface,
                proxy.id,
                proxy.version()
            );
            return Dispatched::BadMsg;
        }
        if message.is_destructor() {
            proxy.object.meta.alive.store(false, Ordering::Release);
            {
                // cleanup the map as appropriate
                let mut map = proxy.map.lock().unwrap();
                let server_destroyed = map
                    .with(proxy.id, |obj| {
                        obj.meta.client_destroyed = true;
                        obj.meta.server_destroyed
                    })
                    .unwrap_or(false);
                if server_destroyed {
                    map.remove(proxy.id);
                }
            }
            (self.implementation)(message, Main::<I>::wrap(proxy));
        } else {
            (self.implementation)(message, Main::<I>::wrap(proxy));
        }
        Dispatched::Yes
    }
}

pub(crate) fn make_dispatcher<I, E>(filter: Filter<E>) -> Arc<Mutex<dyn Dispatcher + Send>>
where
    I: Interface + AsRef<Proxy<I>> + From<Proxy<I>> + Sync,
    E: From<(Main<I>, I::Event)> + 'static,
    I::Event: MessageGroup<Map = ProxyMap>,
{
    let guard = ThreadGuard::new(filter);
    Arc::new(Mutex::new(ImplDispatcher {
        _i: ::std::marker::PhantomData,
        implementation: move |evt, proxy| guard.get().send((proxy, evt).into()),
    }))
}

pub(crate) fn default_dispatcher() -> Arc<Mutex<dyn Dispatcher + Send>> {
    struct DefaultDisp;
    impl Dispatcher for DefaultDisp {
        fn dispatch(&mut self, msg: Message, proxy: ProxyInner, _map: &mut ProxyMap) -> Dispatched {
            Dispatched::NoDispatch(msg, proxy)
        }
    }

    Arc::new(Mutex::new(DefaultDisp))
}
