use zbus::{ObjectServer, interface};
use zvariant::{ObjectPath, OwnedObjectPath};

pub struct Request {
    handle_path: OwnedObjectPath,
}

impl Request {
    pub async fn register(server: &ObjectServer, handle_path: &ObjectPath<'_>) -> zbus::Result<()> {
        let request = Request {
            handle_path: handle_path.to_owned().into(),
        };

        if server.at(handle_path, request).await? {
            Ok(())
        } else {
            Err(zbus::Error::Failure("request already exists".into()))
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    pub async fn close(
        &mut self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> zbus::fdo::Result<()> {
        server.remove::<Self, _>(&self.handle_path).await?;
        Ok(())
    }
}
