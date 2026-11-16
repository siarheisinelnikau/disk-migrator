//! Explicit advanced command entry points. Parsing and help never open a device.
use crate::{Result, invalid, physical::CopyMode};
use std::{collections::BTreeMap, path::PathBuf};

pub const HELP: &str = "Disk Migrator advanced commands (experimental Windows x64)\n\n  --plan-image-to-ssd --source FILE.img --disk N --checkpoint FILE.json [--sector 512|4096]\n  --migrate-image-to-ssd --source FILE.img --disk N --checkpoint FILE.json --confirm \"ERASE PhysicalDriveN SERIAL\" [--resume|--restart]\n  --plan-windows-to-ssd --source-disk N --disk M --checkpoint FILE.json\n  --migrate-windows-to-ssd --source-disk N --disk M --checkpoint FILE.json --confirm \"ERASE PhysicalDriveM SERIAL\" [--restart]\n  --capture-windows --source-disk N --destination FILE.img\n\nOptional: --report NEW.json. Commands print JSON to the parent console.\nNo command starts an elevation prompt or changes disk online/read-only state.\nThe SSD must already be offline, writable, large enough, and have matching sectors.\nLive capture requires healthy System/Registry/ASR VSS writers, basic UEFI/GPT, NTFS data/recovery, and an exclusively lockable FAT32 EFI volume. BitLocker, ReFS, dynamic disks, shrinking and boot repair are unsupported.\nLive snapshots are ephemeral; export an offline image for resume across app restarts.\nBootability and filesystem health are never inferred from byte verification.\n";
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Help,
    PlanImage,
    MigrateImage,
    PlanWindows,
    MigrateWindows,
    CaptureWindows,
}
#[derive(Clone, Debug)]
pub struct Command {
    pub kind: CommandKind,
    pub source: Option<PathBuf>,
    pub destination: Option<PathBuf>,
    pub source_disk: Option<u32>,
    pub disk: Option<u32>,
    pub checkpoint: Option<PathBuf>,
    pub confirmation: Option<String>,
    pub sector: u32,
    pub mode: CopyMode,
    pub report: Option<PathBuf>,
}
pub fn parse(args: &[String]) -> Result<Option<Command>> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    let kind = match first.as_str() {
        "--help" | "-h" => CommandKind::Help,
        "--plan-image-to-ssd" => CommandKind::PlanImage,
        "--migrate-image-to-ssd" => CommandKind::MigrateImage,
        "--plan-windows-to-ssd" => CommandKind::PlanWindows,
        "--migrate-windows-to-ssd" => CommandKind::MigrateWindows,
        "--capture-windows" => CommandKind::CaptureWindows,
        _ => {
            return Err(invalid(
                "Unknown command. Use --help for explicit image/VSS/SSD operations.",
            ));
        }
    };
    let mut fields = BTreeMap::new();
    let mut mode = CopyMode::Start;
    let mut i = 1;
    while i < args.len() {
        let key = &args[i];
        if ["--resume", "--restart"].contains(&key.as_str()) {
            if mode != CopyMode::Start {
                return Err(invalid("Choose one copy mode."));
            }
            mode = if key == "--resume" {
                CopyMode::Resume
            } else {
                CopyMode::Restart
            };
            i += 1;
            continue;
        }
        if ![
            "--source",
            "--destination",
            "--source-disk",
            "--disk",
            "--checkpoint",
            "--confirm",
            "--sector",
            "--report",
        ]
        .contains(&key.as_str())
            || i + 1 >= args.len()
            || fields.insert(key.clone(), args[i + 1].clone()).is_some()
        {
            return Err(invalid(
                "Unknown, duplicate or incomplete command argument.",
            ));
        }
        i += 2;
    }
    let allowed: &[&str] = match kind {
        CommandKind::Help => &[],
        CommandKind::PlanImage => &["--source", "--disk", "--checkpoint", "--sector", "--report"],
        CommandKind::MigrateImage => &[
            "--source",
            "--disk",
            "--checkpoint",
            "--sector",
            "--confirm",
            "--report",
        ],
        CommandKind::PlanWindows => &["--source-disk", "--disk", "--checkpoint", "--report"],
        CommandKind::MigrateWindows => &[
            "--source-disk",
            "--disk",
            "--checkpoint",
            "--confirm",
            "--report",
        ],
        CommandKind::CaptureWindows => &["--source-disk", "--destination", "--report"],
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(
            "An argument belongs to a different command; review the selected operation.",
        ));
    }
    let number = |key: &str| -> Result<Option<u32>> {
        fields
            .get(key)
            .map(|s| {
                s.parse::<u32>()
                    .ok()
                    .filter(|n| *n <= 4095)
                    .ok_or_else(|| invalid("Disk number must be an integer from 0 to 4095."))
            })
            .transpose()
    };
    let sector = fields
        .get("--sector")
        .map(|s| s.parse::<u32>())
        .transpose()
        .map_err(|_| invalid("Invalid sector size."))?
        .unwrap_or(512);
    if ![512, 4096].contains(&sector) {
        return Err(invalid("Logical sector size must be 512 or 4096."));
    }
    let result = Command {
        kind,
        source: fields.get("--source").map(PathBuf::from),
        destination: fields.get("--destination").map(PathBuf::from),
        source_disk: number("--source-disk")?,
        disk: number("--disk")?,
        checkpoint: fields.get("--checkpoint").map(PathBuf::from),
        confirmation: fields.get("--confirm").cloned(),
        sector,
        mode,
        report: fields.get("--report").map(PathBuf::from),
    };
    match result.kind {
        CommandKind::Help if !fields.is_empty() || mode != CopyMode::Start => {
            return Err(invalid("Help does not accept device or copy arguments."));
        }
        CommandKind::PlanImage | CommandKind::MigrateImage
            if result.source.is_none() || result.disk.is_none() || result.checkpoint.is_none() =>
        {
            return Err(invalid(
                "Image-to-SSD commands require source, disk and checkpoint.",
            ));
        }
        CommandKind::PlanWindows | CommandKind::MigrateWindows
            if result.source_disk.is_none()
                || result.disk.is_none()
                || result.checkpoint.is_none() =>
        {
            return Err(invalid(
                "Windows-to-SSD commands require source-disk, disk and checkpoint.",
            ));
        }
        CommandKind::CaptureWindows
            if result.source_disk.is_none() || result.destination.is_none() =>
        {
            return Err(invalid(
                "VSS capture requires source-disk and a new destination image.",
            ));
        }
        _ => {}
    }
    if matches!(
        result.kind,
        CommandKind::MigrateImage | CommandKind::MigrateWindows
    ) && result.confirmation.is_none()
    {
        return Err(invalid(
            "Migration requires an exact explicit --confirm string from the reviewed plan.",
        ));
    }
    if matches!(
        result.kind,
        CommandKind::PlanImage | CommandKind::PlanWindows | CommandKind::CaptureWindows
    ) && result.mode != CopyMode::Start
    {
        return Err(invalid("Copy-mode flags are only valid for migration."));
    }
    if result.kind == CommandKind::MigrateWindows && result.mode == CopyMode::Resume {
        return Err(invalid(
            "Live VSS snapshots cannot be recreated for resume. Resume from an exported offline image, or explicitly --restart.",
        ));
    }
    Ok(Some(result))
}

#[cfg(windows)]
pub fn run(command: Command) -> Result<()> {
    use crate::{
        capture,
        physical::{self, CopyRequest, FileCheckpoint, PhysicalPlan, TargetFactory},
        windows::{
            device::{ImageSource, WindowsTargetFactory},
            live::LiveInspection,
        },
    };
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::atomic::{AtomicBool, Ordering},
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT,
        SetConsoleCtrlHandler,
    };
    static CANCEL: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn ctrl(kind: u32) -> i32 {
        if [CTRL_C_EVENT, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT].contains(&kind) {
            CANCEL.store(true, Ordering::Relaxed);
            1
        } else {
            0
        }
    }
    struct CtrlGuard;
    impl Drop for CtrlGuard {
        fn drop(&mut self) {
            unsafe { SetConsoleCtrlHandler(Some(ctrl), 0) };
        }
    }
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
        SetConsoleCtrlHandler(Some(ctrl), 1);
    }
    let _guard = CtrlGuard;
    if let Some(path) = &command.checkpoint {
        crate::image::validate_state_path(path)?;
    }
    if let Some(path) = &command.report {
        crate::image::validate_state_path(path)?;
        if path.parent().is_none_or(|p| !p.is_dir()) {
            return Err(invalid("Report parent directory does not exist."));
        }
        if path.exists() {
            return Err(invalid(
                "A report path must name a new file; existing reports are preserved.",
            ));
        }
    }
    if command.kind == CommandKind::Help {
        print!("{HELP}");
        return Ok(());
    }
    let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut progress = |p: crate::migration::Progress| {
        if last.elapsed() > std::time::Duration::from_millis(250) || p.done == p.total {
            last = std::time::Instant::now();
            eprintln!("{:?}: {} / {}", p.phase, p.done, p.total);
        }
    };
    let output = match command.kind {
        CommandKind::PlanImage | CommandKind::MigrateImage => {
            let path = command.source.as_ref().unwrap();
            let mut source = ImageSource::open(path, command.sector)?;
            let checkpoint = command.checkpoint.as_ref().unwrap();
            let mut factory =
                WindowsTargetFactory::new(command.disk.unwrap(), vec![path.clone()], checkpoint)?;
            let target = factory.inspect()?;
            let plan = PhysicalPlan::build(&source, target)?;
            if command.kind == CommandKind::PlanImage {
                serde_json::json!({"plan":plan,"confirmation":plan.confirmation(),"bootability":"not_tested","physical_integration":"unverified"})
            } else {
                let mut store = FileCheckpoint::new(checkpoint.clone());
                let report = physical::execute(
                    &mut source,
                    &mut factory,
                    &mut store,
                    CopyRequest {
                        plan: &plan,
                        confirmation: command.confirmation.as_deref().unwrap(),
                        mode: command.mode,
                        cancel: &CANCEL,
                    },
                    &mut progress,
                )?;
                serde_json::to_value(report)?
            }
        }
        CommandKind::PlanWindows | CommandKind::MigrateWindows => {
            let inspection = LiveInspection::inspect(command.source_disk.unwrap())?;
            let checkpoint = command.checkpoint.as_ref().unwrap();
            let mut factory = WindowsTargetFactory::new(command.disk.unwrap(), vec![], checkpoint)?;
            let target = factory.inspect()?;
            physical::validate_target_disk(
                std::slice::from_ref(&inspection.disk.hardware_id),
                inspection.disk.bytes,
                inspection.disk.sector,
                &target,
            )?;
            if inspection.disk.number == target.disk.number {
                return Err(invalid("Live source and target disk are identical."));
            }
            let confirmation = physical::confirmation_for(&target.disk);
            if command.kind == CommandKind::PlanWindows {
                serde_json::json!({"source":inspection.disk,"layout":inspection.layout,"destination":target,"confirmation":confirmation,"vss":"not_created","requirements":"healthy writers and an exclusively lockable FAT32 ESP","bootability":"not_tested"})
            } else {
                if command.confirmation.as_deref() != Some(&confirmation) {
                    return Err(invalid(
                        "Destination confirmation does not match this hardware serial.",
                    ));
                }
                if command.mode == CopyMode::Start && checkpoint.exists() {
                    return Err(invalid(
                        "A prior live checkpoint exists. Use an offline image to resume or explicitly --restart with a fresh VSS set.",
                    ));
                }
                let mut source = inspection.prepare(&CANCEL)?;
                let plan = PhysicalPlan::build(&source, target)?;
                let mut store = FileCheckpoint::new(checkpoint.clone());
                let result = physical::execute(
                    &mut source,
                    &mut factory,
                    &mut store,
                    CopyRequest {
                        plan: &plan,
                        confirmation: &confirmation,
                        mode: command.mode,
                        cancel: &CANCEL,
                    },
                    &mut progress,
                );
                match result {
                    Ok(report) => {
                        source.reader.session.finish(&CANCEL)?;
                        serde_json::to_value(report)?
                    }
                    Err(e) => {
                        source.reader.session.close_failed().map_err(|cleanup| {
                            invalid(format!("{e}; VSS cleanup failed: {cleanup}"))
                        })?;
                        return Err(e);
                    }
                }
            }
        }
        CommandKind::CaptureWindows => {
            let destination = command.destination.as_ref().unwrap();
            crate::image::validate_path(destination)?;
            if destination.exists() {
                return Err(invalid("VSS capture never overwrites an existing image."));
            }
            let mut source =
                LiveInspection::inspect(command.source_disk.unwrap())?.prepare(&CANCEL)?;
            let result = capture::capture_image(&mut source, destination, &CANCEL, &mut progress);
            match result {
                Ok(report) => {
                    source.reader.session.finish(&CANCEL)?;
                    serde_json::to_value(report)?
                }
                Err(e) => {
                    source.reader.session.close_failed().map_err(|cleanup| {
                        invalid(format!("{e}; VSS cleanup failed: {cleanup}"))
                    })?;
                    return Err(e);
                }
            }
        }
        CommandKind::Help => unreachable!(),
    };
    let json = serde_json::to_vec_pretty(&output)?;
    println!("{}", String::from_utf8_lossy(&json));
    if let Some(path) = command.report {
        if !path.is_absolute() || path.extension().is_none_or(|e| e != "json") {
            return Err(invalid("Report must be a new absolute .json file."));
        }
        let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    Ok(())
}
