use disk_migrator::{
    cli::{self, CommandKind},
    image,
};
fn parse(text: &[&str]) -> disk_migrator::Result<Option<cli::Command>> {
    cli::parse(&text.iter().map(|s| (*s).into()).collect::<Vec<_>>())
}
#[test]
fn help_and_empty_launch_parse_without_initializing_any_adapter() {
    assert!(parse(&[]).unwrap().is_none());
    assert_eq!(parse(&["--help"]).unwrap().unwrap().kind, CommandKind::Help);
}
#[test]
fn physical_commands_require_an_explicit_destination_and_confirmation() {
    assert!(
        parse(&[
            "--migrate-image-to-ssd",
            "--source",
            "source.img",
            "--disk",
            "7",
            "--checkpoint",
            "cp.json"
        ])
        .is_err()
    );
    assert!(
        parse(&[
            "--migrate-windows-to-ssd",
            "--source-disk",
            "0",
            "--disk",
            "7",
            "--checkpoint",
            "cp.json",
            "--confirm",
            "ERASE PhysicalDrive7 SSD-TEST",
            "--resume"
        ])
        .is_err()
    );
    assert!(
        parse(&[
            "--plan-image-to-ssd",
            "--source",
            "source.img",
            "--disk",
            "7",
            "--checkpoint",
            "cp.json"
        ])
        .is_ok()
    );
}
#[test]
fn unknown_duplicate_or_incomplete_arguments_fail_closed() {
    for args in [
        vec!["--unknown"],
        vec!["--help", "--disk"],
        vec!["--plan-image-to-ssd", "--disk", "1", "--disk", "2"],
        vec![
            "--plan-windows-to-ssd",
            "--source-disk",
            "5000",
            "--disk",
            "2",
            "--checkpoint",
            "cp.json",
        ],
    ] {
        assert!(parse(&args).is_err());
    }
}
#[test]
fn capture_cannot_silently_accept_an_unrelated_destination_disk() {
    assert!(
        parse(&[
            "--capture-windows",
            "--source-disk",
            "0",
            "--destination",
            "capture.img",
            "--disk",
            "2"
        ])
        .is_err()
    );
}
#[test]
fn checkpoint_paths_reject_device_namespaces_and_alternate_data_streams() {
    assert!(image::validate_state_path(std::path::Path::new(r"\\.\PhysicalDrive0")).is_err());
    #[cfg(windows)]
    {
        assert!(image::validate_state_path(std::path::Path::new(r"C:\NUL.json")).is_err());
        assert!(
            image::validate_state_path(std::path::Path::new(r"C:\image.img:state.json")).is_err()
        );
    }
}
