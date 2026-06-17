use std::collections::HashMap;

use serde::{Serialize, Serializer};
use zvariant::{OwnedValue, Type, Value};

pub mod request;
pub mod screen_cast;
pub mod session;

pub static PORTAL_DBUS_NAME: &str = "org.freedesktop.impl.portal.desktop.shiny";
pub static PORTAL_DBUS_PATH: &str = "/org/freedesktop/portal/desktop";

#[derive(Type)]
#[zvariant(signature = "(ua{sv})")]
enum PortalResponse<T: Type + Serialize = HashMap<String, OwnedValue>> {
    Success(T),
    Cancelled,
    Other,
}

impl<T: Type + Serialize> Serialize for PortalResponse<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Success(result) => (0, result).serialize(serializer),
            Self::Cancelled => (1, HashMap::<String, Value>::new()).serialize(serializer),
            Self::Other => (2, HashMap::<String, Value>::new()).serialize(serializer),
        }
    }
}
