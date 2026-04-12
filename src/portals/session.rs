use futures_util::{FutureExt, future::BoxFuture};
use zbus::{ObjectServer, interface, object_server::SignalEmitter};
use zvariant::{ObjectPath, OwnedObjectPath};

pub struct Session<T: Send + Sync + 'static> {
    handle_path: OwnedObjectPath,
    pub inner: T,
    cleanup_fn: Option<Box<dyn FnOnce(&mut T) -> BoxFuture<'static, ()> + Send + Sync + 'static>>,
}

impl<T: Send + Sync + 'static> Session<T> {
    pub async fn register<F, Fut>(
        server: &ObjectServer,
        handle_path: &ObjectPath<'_>,
        inner: T,
        cleanup_fn: F,
    ) -> zbus::Result<()>
    where
        F: FnOnce(&mut T) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let session = Session {
            handle_path: handle_path.to_owned().into(),
            inner,
            cleanup_fn: Some(Box::new(move |d| cleanup_fn(d).boxed())),
        };

        if server.at(handle_path, session).await? {
            Ok(())
        } else {
            Err(zbus::Error::Failure("interface already exists".into()))
        }
    }

    pub async fn get(
        server: &ObjectServer,
        handle_path: &ObjectPath<'_>,
    ) -> Option<zbus::object_server::InterfaceRef<Self>> {
        server.interface::<_, Self>(handle_path).await.ok()
    }
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl<T: Send + Sync + 'static> Session<T> {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        1
    }

    pub async fn close(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        server.remove::<Self, _>(&self.handle_path).await?;
        Self::closed(&emitter).await?;

        if let Some(cleanup_fn) = self.cleanup_fn.take() {
            cleanup_fn(&mut self.inner).await;
        }

        Ok(())
    }

    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
