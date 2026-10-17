use super::{
    BacklightDevice, BacklightKind, ddc_raw_value, parse_ddc_brightness, select_backlight_device,
};

fn backlight(name: &str, kind: BacklightKind) -> BacklightDevice {
    BacklightDevice {
        name: name.to_owned(),
        kind,
        brightness: 40,
        maximum: 100,
    }
}

#[test]
fn selects_backlight_using_kernel_interface_priority() {
    let selected = select_backlight_device(vec![
        backlight("intel_backlight", BacklightKind::Raw),
        backlight("acpi_video0", BacklightKind::Firmware),
        backlight("vendor", BacklightKind::Platform),
    ])
    .expect("a backlight should be selected");

    assert_eq!(selected.name, "acpi_video0");
}

#[test]
fn selects_backlight_deterministically_within_same_kind() {
    let selected = select_backlight_device(vec![
        backlight("backlight_b", BacklightKind::Raw),
        backlight("backlight_a", BacklightKind::Raw),
    ])
    .expect("a backlight should be selected");

    assert_eq!(selected.name, "backlight_a");
}

#[test]
fn parses_ddcutil_brief_continuous_value() {
    assert_eq!(parse_ddc_brightness("VCP 10 C 57 100"), Some((0.57, 100)));
    assert_eq!(
        parse_ddc_brightness("Display 1\nVCP 10 C 80 100"),
        Some((0.8, 100))
    );
    assert_eq!(
        parse_ddc_brightness("VCP 10 C invalid 100\nVCP 10 C 65 100"),
        Some((0.65, 100))
    );
    assert_eq!(parse_ddc_brightness("VCP 10 C NaN 100"), None);
    assert_eq!(parse_ddc_brightness("VCP 10 C 20 NaN"), None);
    assert_eq!(parse_ddc_brightness("VCP 10 C -1 100"), None);
    assert_eq!(parse_ddc_brightness("VCP 10 C 1 0"), None);
    assert_eq!(parse_ddc_brightness("VCP 10 C 1 65536"), None);
}

#[test]
fn ddc_percentages_use_the_displays_native_range() {
    assert_eq!(
        parse_ddc_brightness("VCP 10 C 64 255"),
        Some((64.0 / 255.0, 255))
    );
    assert_eq!(ddc_raw_value(50, 100), 50);
    assert_eq!(ddc_raw_value(50, 255), 128);
    assert_eq!(ddc_raw_value(100, 255), 255);
    assert_eq!(ddc_raw_value(5, 255), 13);
    assert_eq!(ddc_raw_value(0, 100), 5);
    assert_eq!(ddc_raw_value(-100, 100), 5);
    assert_eq!(ddc_raw_value(5, 127), 7);
    assert_eq!(ddc_raw_value(100, u16::MAX), u16::MAX);
}

#[test]
fn no_supported_ddc_range_can_round_below_five_percent() {
    for maximum in 1..=u16::MAX {
        let minimum = ddc_raw_value(-1, maximum);
        assert!(u32::from(minimum) * 100 >= u32::from(maximum) * 5);
        assert!(minimum <= maximum);
    }
    for input in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0, 0.049] {
        assert_eq!(super::clamp_brightness(input), super::BRIGHTNESS_MIN);
    }
    assert_eq!(super::native_brightness_value(100, u64::MAX), u64::MAX);
    assert_eq!(super::native_brightness_value(0, 127), 7);
}

#[test]
fn laptop_backlight_does_not_probe_ddc() {
    let root = std::env::temp_dir().join(format!("obsidian-backlight-{}", std::process::id()));
    let device = root.join("intel_backlight");
    std::fs::create_dir_all(&device).unwrap();
    for (name, value) in [
        ("brightness", "400"),
        ("max_brightness", "800"),
        ("type", "raw"),
    ] {
        std::fs::write(device.join(name), value).unwrap();
    }
    let state =
        super::read_brightness_at(&root, || panic!("laptop must not start ddcutil")).unwrap();
    assert_eq!(state.backend, super::BrightnessBackend::Backlight);
    assert_eq!(state.value, 0.5);
    std::fs::write(device.join("max_brightness"), "0").unwrap();
    let fallback = super::read_brightness_at(&root, || Err("DDC unavailable".into()));
    assert!(fallback.is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ddc_discovery_respects_the_first_valid_display() {
    assert_eq!(
        super::parse_ddc_bus(
            "Invalid display\n I2C bus: /dev/i2c-2\nDisplay 1\n I2C bus: /dev/i2c-37\nDisplay 2\n I2C bus: /dev/i2c-42"
        ),
        Some(37)
    );
    assert_eq!(
        super::parse_ddc_bus("Display 1\n USB bus: 1.2\nDisplay 2\n I2C bus: /dev/i2c-42"),
        None
    );
    assert_eq!(super::parse_ddc_bus("No displays found"), None);
    assert_eq!(
        super::parse_ddc_bus("Display 1\n I2C bus: /dev/i2c-invalid"),
        None
    );
}

#[test]
fn ddc_fast_path_preserves_fallbacks_verification_and_error_recovery() {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const ROOT_VARIABLE: &str = "OBSIDIAN_BRIGHTNESS_TEST_ROOT";
    if let Some(root) = std::env::var_os(ROOT_VARIABLE) {
        let root = std::path::PathBuf::from(root);
        for _ in 0..2 {
            assert_eq!(read_ddc().unwrap(), (128.0 / 255.0, 255, Some(37)));
        }
        let backend = BrightnessBackend::Ddc {
            maximum: 255,
            bus: Some(37),
        };
        write_brightness(backend, 0.0).unwrap();
        let calls = fs::read_to_string(root.join("calls")).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("detect "))
                .count(),
            1
        );
        assert!(calls.contains("setvcp 10 13 --bus 37 --skip-ddc-checks"));
        assert!(!calls.contains("--noverify"));

        fs::write(root.join("fail-write"), "").unwrap();
        assert!(write_brightness(backend, 0.5).is_err());
        assert!(BRIGHTNESS_CACHE.get().is_none());
        assert!(DDC_DISCOVERY.lock().unwrap().is_none());
        fs::remove_file(root.join("fail-write")).unwrap();
        read_ddc().unwrap();
        DDC_DISCOVERY
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .topology
            .push(("removed-output".into(), vec![]));
        let calls_before = fs::read_to_string(root.join("calls")).unwrap();
        assert!(write_brightness(backend, 0.5).is_err());
        assert_eq!(
            fs::read_to_string(root.join("calls")).unwrap(),
            calls_before
        );

        DDC_DISCOVERY.lock().unwrap().take();
        fs::write(root.join("legacy"), "").unwrap();
        assert_eq!(read_ddc().unwrap().2, None);
        assert_eq!(read_ddc().unwrap().2, None);
        write_brightness(
            BrightnessBackend::Ddc {
                maximum: 255,
                bus: None,
            },
            0.5,
        )
        .unwrap();
        assert!(
            fs::read_to_string(root.join("calls"))
                .unwrap()
                .lines()
                .any(|line| line == "setvcp 10 128")
        );
        return;
    }
    let root = std::env::temp_dir().join(format!("obsidian-ddc-fixture-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let program = root.join("ddcutil-fixture");
    fs::write(
        &program,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/calls"
case "$1" in
detect) printf 'Display 1\n I2C bus: /dev/i2c-37\n' ;;
getvcp)
  if [ -f "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/legacy" ]; then
case " $* " in *' --skip-ddc-checks '*) exit 2 ;; esac
  fi
  printf 'VCP 10 C 128 255\n'
  ;;
setvcp) if [ -f "$OBSIDIAN_BRIGHTNESS_TEST_ROOT/fail-write" ]; then exit 1; fi ;;
*) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "widgets::brightness::backend::tests::ddc_fast_path_preserves_fallbacks_verification_and_error_recovery", "--test-threads=1", "--nocapture"])
        .env(ROOT_VARIABLE, &root)
        .env("OBSIDIAN_BAR_DDCUTIL_BIN", &program)
        .output().unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
