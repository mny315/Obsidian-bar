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

/// Inspect property names without unpacking large metadata or pixmap values.
pub(super) fn properties_changed<'a>(
    changed: &glib::Variant,
    invalidated: impl IntoIterator<Item = &'a str>,
    tracked: &[&str],
) -> bool {
    if invalidated.into_iter().any(|name| tracked.contains(&name))
        || changed.type_() != glib::VariantTy::VARDICT
    {
        return true;
    }
    changed.iter().any(|entry| {
        entry
            .child_value(0)
            .str()
            .is_none_or(|name| tracked.contains(&name))
    })
}

pub(super) fn property_signal_changed(
    parameters: &glib::Variant,
    tracked: impl FnOnce(&str) -> &'static [&'static str],
) -> bool {
    if parameters.type_().as_str() != "(sa{sv}as)" {
        return true;
    }
    let interface = parameters.child_value(0);
    let tracked = tracked(interface.str().expect("validated interface string"));
    if tracked.is_empty() {
        return false;
    }
    let invalidated = parameters
        .child_value(2)
        .get::<Vec<String>>()
        .expect("validated invalidated property list");
    properties_changed(
        &parameters.child_value(1),
        invalidated.iter().map(String::as_str),
        tracked,
    )
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

    #[test]
    fn property_notifications_ignore_unused_values_and_keep_invalidations() {
        use std::collections::HashMap;
        let changed = HashMap::from([("Position", 42_i64.to_variant())]).to_variant();
        assert!(!properties_changed(
            &changed,
            [],
            &["Metadata", "PlaybackStatus"]
        ));
        assert!(properties_changed(
            &changed,
            ["Metadata"],
            &["Metadata", "PlaybackStatus"]
        ));
        assert!(properties_changed(&changed, [], &["Position"]));
        assert!(!properties_changed(
            &HashMap::<String, glib::Variant>::new().to_variant(),
            ["Volume"],
            &["Metadata"]
        ));
        assert!(properties_changed(&42_i32.to_variant(), [], &["Metadata"]));
    }
}
