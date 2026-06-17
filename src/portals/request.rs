use futures_util::{FutureExt, future::BoxFuture};
use zbus::{ObjectServer, interface};
use zvariant::{ObjectPath, OwnedObjectPath};

pub struct Request<T: Send + Sync + 'static> {
    handle_path: OwnedObjectPath,
    pub inner: T,
    cleanup_fn: Option<Box<dyn FnOnce(&mut T) -> BoxFuture<'static, ()> + Send + Sync + 'static>>,
}

impl<T: Send + Sync + 'static> Request<T> {
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
        let request = Request {
            handle_path: handle_path.to_owned().into(),
            inner,
            cleanup_fn: Some(Box::new(move |data| cleanup_fn(data).boxed())),
        };

        if server.at(handle_path, request).await? {
            Ok(())
        } else {
            Err(zbus::Error::Failure("request already exists".into()))
        }
    }

    #[allow(dead_code)]
    pub async fn get(
        server: &ObjectServer,
        handle_path: &ObjectPath<'_>,
    ) -> Option<zbus::object_server::InterfaceRef<Self>> {
        server.interface::<_, Self>(handle_path).await.ok()
    }
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl<T: Send + Sync + 'static> Request<T> {
    pub async fn close(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<()> {
        server.remove::<Self, _>(&self.handle_path).await?;

        if let Some(cleanup_fn) = self.cleanup_fn.take() {
            cleanup_fn(&mut self.inner).await;
        }

        Ok(())
    }
}
