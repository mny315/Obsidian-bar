use gio::glib;
use glib::variant::{FromVariant, ObjectPath};

pub(super) fn variant_value<T: FromVariant>(value: &glib::Variant) -> Option<T> {
    if let Some(value) = value.get::<T>() {
        return Some(value);
    }
    if !value.is::<glib::Variant>() {
        return None;
    }
    value.as_variant().and_then(|inner| inner.get::<T>())
}

pub(super) fn unbox_variant(value: &glib::Variant) -> glib::Variant {
    if value.is::<glib::Variant>() {
        value.as_variant().unwrap_or_else(|| value.clone())
    } else {
        value.clone()
    }
}

pub(super) fn object_path(path: &str) -> Result<ObjectPath, String> {
    ObjectPath::try_from(path)
        .map_err(|error| format!("invalid D-Bus object path {path:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use glib::variant::ToVariant;

    use super::*;

    #[test]
    fn reads_plain_and_boxed_variants_without_forced_unboxing() {
        let plain = 42_i32.to_variant();
        let boxed = plain.to_variant();

        assert_eq!(variant_value::<i32>(&plain), Some(42));
        assert_eq!(variant_value::<i32>(&boxed), Some(42));
        assert_eq!(unbox_variant(&plain), plain);
        assert_eq!(unbox_variant(&boxed), plain);
    }
}
