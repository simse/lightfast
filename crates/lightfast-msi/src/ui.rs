//! The installer's dialogs: a small wizard in the style of `WiX`'s minimal UI.
//!
//! Installing shows Welcome → Progress → Finish, with a "Start Lightfast"
//! check box. Opening the MSI again when this exact build is installed shows
//! a dialog offering to remove it. `WinSparkle` runs updates with `/passive`,
//! which skips every dialog here and shows only Windows Installer's own
//! progress bar.

use std::io::{self, Seek, Write};

use msi::{Column, Package, Value};

use crate::package::{i16_, i16_opt, i32_opt, id, id_opt, key, loc_opt, n, s, table, text, text_opt};

/// The `TextStyle` dialogs use for plain text.
pub const DEFAULT_FONT: &str = "Normal";
/// The custom action that starts the installed app.
pub const LAUNCH_ACTION: &str = "LaunchApplication";

/// The dialogs' places in `InstallUISequence`: the first dialog after
/// costing, the progress dialog just before `ExecuteAction` (at 1300) runs
/// the install, and the end dialogs by their reserved negative numbers.
pub const SEQUENCE: [(&str, Option<&str>, i32); 6] = [
    ("WelcomeDlg", Some("NOT Installed"), 1230),
    ("MaintenanceDlg", Some("Installed"), 1231),
    ("ProgressDlg", None, 1280),
    ("ExitDlg", None, -1),
    ("UserExitDlg", None, -2),
    ("FatalErrorDlg", None, -3),
];

const WIDTH: i32 = 370;
const HEIGHT: i32 = 270;

/// Control attributes.
const VISIBLE: i32 = 1;
const ENABLED: i32 = 2;
/// A progress bar drawn as a bar, not as blocks.
const PROGRESS95: i32 = 65_536;
/// The UAC shield, on buttons that start an elevated install.
const ELEVATION_SHIELD: i32 = 0x0080_0000;
/// Dialog attributes: modal.
const MODAL: i32 = 2;

/// One row of the `Control` table.
struct Control {
    name: &'static str,
    kind: &'static str,
    rect: (i32, i32, i32, i32),
    attributes: i32,
    property: Option<&'static str>,
    text: Option<&'static str>,
    next: Option<&'static str>,
}

const fn control(name: &'static str, kind: &'static str, rect: (i32, i32, i32, i32), text: &'static str) -> Control {
    Control { name, kind, rect, attributes: VISIBLE | ENABLED, property: None, text: Some(text), next: None }
}

/// A dialog's title, in the title style.
const fn title(text: &'static str) -> Control {
    control("Title", "Text", (20, 20, 330, 40), text)
}

/// A dialog's body text.
const fn body(name: &'static str, text: &'static str) -> Control {
    control(name, "Text", (20, 70, 330, 60), text)
}

/// The line above a dialog's buttons.
const fn line() -> Control {
    Control {
        name: "BottomLine",
        kind: "Line",
        rect: (0, 234, WIDTH, 0),
        attributes: VISIBLE,
        property: None,
        text: None,
        next: None,
    }
}

/// A button at the bottom right: slot 0 is the rightmost.
const fn button(name: &'static str, slot: i32, text: &'static str, next: &'static str) -> Control {
    Control {
        name,
        kind: "PushButton",
        rect: (290 - slot * 78, 243, 66, 17),
        attributes: VISIBLE | ENABLED,
        property: None,
        text: Some(text),
        next: Some(next),
    }
}

/// One row of the `Dialog` table, with its controls.
struct Dialog {
    name: &'static str,
    modal: bool,
    first: &'static str,
    default: Option<&'static str>,
    cancel: Option<&'static str>,
    controls: Vec<Control>,
}

#[expect(clippy::too_many_lines, reason = "one table after another reads best in order")]
fn dialogs() -> Vec<Dialog> {
    let mut install = button("Install", 1, "&Install", "Cancel");
    install.attributes |= ELEVATION_SHIELD;
    let mut remove = button("Remove", 1, "&Remove", "Close");
    remove.attributes |= ELEVATION_SHIELD;
    let launch = Control {
        name: "LaunchCheck",
        kind: "CheckBox",
        rect: (20, 140, 330, 17),
        // Shown by `ControlCondition` only after an install.
        attributes: ENABLED,
        property: Some("LAUNCHAPP"),
        text: Some("&Start Lightfast"),
        next: Some("Finish"),
    };
    let mut installed = body("InstalledText", "Lightfast [ProductVersion] is installed. Find it in the Start menu.");
    installed.attributes = ENABLED;
    let mut removed = body("RemovedText", "Lightfast has been removed from your computer.");
    removed.attributes = ENABLED;
    vec![
        Dialog {
            name: "WelcomeDlg",
            modal: true,
            first: "Install",
            default: Some("Install"),
            cancel: Some("Cancel"),
            controls: vec![
                title(r"{\Title}Install Lightfast"),
                body(
                    "Description",
                    "This installs Lightfast [ProductVersion], a flatbed photo and art scanner, in [INSTALLDIR].\
                     \r\n\r\nIt keeps itself up to date. Click Install to continue.",
                ),
                line(),
                install,
                button("Cancel", 0, "Cancel", "Install"),
            ],
        },
        Dialog {
            name: "MaintenanceDlg",
            modal: true,
            first: "Remove",
            default: Some("Close"),
            cancel: Some("Close"),
            controls: vec![
                title(r"{\Title}Lightfast is installed"),
                body("Description", "This version of Lightfast is already installed. Click Remove to uninstall it."),
                line(),
                remove,
                button("Close", 0, "&Close", "Remove"),
            ],
        },
        Dialog {
            name: "ProgressDlg",
            modal: false,
            first: "Cancel",
            default: Some("Cancel"),
            cancel: Some("Cancel"),
            controls: vec![
                title(r"{\Title}Lightfast Setup"),
                Control {
                    name: "ActionText",
                    kind: "Text",
                    rect: (20, 80, 330, 20),
                    attributes: VISIBLE | ENABLED,
                    property: None,
                    text: None,
                    next: None,
                },
                Control {
                    name: "ProgressBar",
                    kind: "ProgressBar",
                    rect: (20, 105, 330, 12),
                    attributes: VISIBLE | PROGRESS95,
                    property: None,
                    text: Some("Progress done"),
                    next: None,
                },
                line(),
                button("Cancel", 0, "Cancel", "Cancel"),
            ],
        },
        Dialog {
            name: "CancelDlg",
            modal: true,
            first: "No",
            default: Some("No"),
            cancel: Some("No"),
            controls: vec![
                control("Text", "Text", (20, 20, 220, 40), "Are you sure you want to cancel Lightfast setup?"),
                Control { rect: (60, 70, 56, 17), ..button("Yes", 0, "&Yes", "No") },
                Control { rect: (140, 70, 56, 17), ..button("No", 0, "&No", "Yes") },
            ],
        },
        Dialog {
            name: "ExitDlg",
            modal: true,
            first: "Finish",
            default: Some("Finish"),
            cancel: Some("Finish"),
            controls: vec![
                title(r"{\Title}Lightfast Setup is complete"),
                installed,
                removed,
                launch,
                line(),
                button("Finish", 0, "&Finish", "LaunchCheck"),
            ],
        },
        Dialog {
            name: "UserExitDlg",
            modal: true,
            first: "Finish",
            default: Some("Finish"),
            cancel: Some("Finish"),
            controls: vec![
                title(r"{\Title}Lightfast Setup was cancelled"),
                body("Description", "Nothing on your computer has been changed."),
                line(),
                button("Finish", 0, "&Finish", "Finish"),
            ],
        },
        Dialog {
            name: "FatalErrorDlg",
            modal: true,
            first: "Finish",
            default: Some("Finish"),
            cancel: Some("Finish"),
            controls: vec![
                title(r"{\Title}Lightfast Setup failed"),
                body("Description", "Setup ended because of an error. Nothing on your computer has been changed."),
                line(),
                button("Finish", 0, "&Finish", "Finish"),
            ],
        },
    ]
}

/// What each button does: `(dialog, control, event, argument, condition)`,
/// run in the order listed.
const EVENTS: [(&str, &str, &str, &str, &str); 12] = [
    ("WelcomeDlg", "Install", "EndDialog", "Return", "1"),
    ("WelcomeDlg", "Cancel", "SpawnDialog", "CancelDlg", "1"),
    ("MaintenanceDlg", "Remove", "[REMOVE]", "ALL", "1"),
    ("MaintenanceDlg", "Remove", "EndDialog", "Return", "1"),
    ("MaintenanceDlg", "Close", "EndDialog", "Exit", "1"),
    ("ProgressDlg", "Cancel", "SpawnDialog", "CancelDlg", "1"),
    ("CancelDlg", "Yes", "EndDialog", "Exit", "1"),
    ("CancelDlg", "No", "EndDialog", "Return", "1"),
    ("ExitDlg", "Finish", "DoAction", LAUNCH_ACTION, r#"LAUNCHAPP = "1" AND NOT Installed"#),
    ("ExitDlg", "Finish", "EndDialog", "Return", "1"),
    ("UserExitDlg", "Finish", "EndDialog", "Exit", "1"),
    ("FatalErrorDlg", "Finish", "EndDialog", "Exit", "1"),
];

/// Adds the dialog tables.
#[expect(clippy::too_many_lines, reason = "one table after another reads best in order")]
pub fn add<F: io::Read + Write + Seek>(package: &mut Package<F>) -> anyhow::Result<()> {
    table(
        package,
        "TextStyle",
        vec![key("TextStyle", 72), text("FaceName", 32), i16_("Size"), i32_opt("Color"), i16_opt("StyleBits")],
        vec![
            vec![s(DEFAULT_FONT), s("Segoe UI"), n(9), Value::Null, n(0)],
            // Bold.
            vec![s("Title"), s("Segoe UI"), n(13), Value::Null, n(1)],
        ],
    )?;

    let dialogs = dialogs();
    table(
        package,
        "Dialog",
        vec![
            key("Dialog", 72),
            i16_("HCentering"),
            i16_("VCentering"),
            i16_("Width"),
            i16_("Height"),
            i32_opt("Attributes"),
            loc_opt("Title", 128),
            id("Control_First", 50),
            id_opt("Control_Default", 50),
            id_opt("Control_Cancel", 50),
        ],
        dialogs
            .iter()
            .map(|d| {
                let (width, height) = if d.name == "CancelDlg" { (260, 100) } else { (WIDTH, HEIGHT) };
                vec![
                    s(d.name),
                    n(50),
                    n(50),
                    n(width),
                    n(height),
                    n(VISIBLE | if d.modal { MODAL } else { 0 }),
                    s("[ProductName] Setup"),
                    s(d.first),
                    d.default.map_or(Value::Null, s),
                    d.cancel.map_or(Value::Null, s),
                ]
            })
            .collect(),
    )?;

    table(
        package,
        "Control",
        vec![
            key("Dialog_", 72),
            key("Control", 50),
            id("Type", 20),
            i16_("X"),
            i16_("Y"),
            i16_("Width"),
            i16_("Height"),
            i32_opt("Attributes"),
            id_opt("Property", 72),
            loc_opt("Text", 0),
            id_opt("Control_Next", 50),
            loc_opt("Help", 50),
        ],
        dialogs
            .iter()
            .flat_map(|d| {
                d.controls.iter().map(|c| {
                    let (x, y, w, h) = c.rect;
                    vec![
                        s(d.name),
                        s(c.name),
                        s(c.kind),
                        n(x),
                        n(y),
                        n(w),
                        n(h),
                        n(c.attributes),
                        c.property.map_or(Value::Null, s),
                        c.text.map_or(Value::Null, s),
                        c.next.map_or(Value::Null, s),
                        Value::Null,
                    ]
                })
            })
            .collect(),
    )?;

    let mut ordering = std::collections::HashMap::<(&str, &str), i32>::new();
    table(
        package,
        "ControlEvent",
        vec![
            key("Dialog_", 72),
            key("Control_", 50),
            Column::build("Event").primary_key().text_string(50),
            Column::build("Argument").primary_key().text_string(255),
            Column::build("Condition").primary_key().nullable().text_string(255),
            i16_opt("Ordering"),
        ],
        EVENTS
            .iter()
            .map(|&(dialog, control, event, argument, condition)| {
                let order = ordering.entry((dialog, control)).or_insert(0);
                *order += 1;
                vec![s(dialog), s(control), s(event), s(argument), s(condition), n(*order)]
            })
            .collect(),
    )?;

    // The progress dialog follows the install's progress messages.
    table(
        package,
        "EventMapping",
        vec![key("Dialog_", 72), key("Control_", 50), key("Event", 50), text("Attribute", 50)],
        vec![
            vec![s("ProgressDlg"), s("ActionText"), s("ActionText"), s("Text")],
            vec![s("ProgressDlg"), s("ProgressBar"), s("SetProgress"), s("Progress")],
        ],
    )?;

    // The finish dialog says what happened: `Installed` is set when the
    // product was installed before this session, so only for a removal.
    table(
        package,
        "ControlCondition",
        vec![
            key("Dialog_", 72),
            key("Control_", 50),
            key("Action", 50),
            Column::build("Condition").primary_key().text_string(255),
        ],
        vec![
            vec![s("ExitDlg"), s("InstalledText"), s("Show"), s("NOT Installed")],
            vec![s("ExitDlg"), s("LaunchCheck"), s("Show"), s("NOT Installed")],
            vec![s("ExitDlg"), s("RemovedText"), s("Show"), s("Installed")],
        ],
    )?;

    table(package, "CheckBox", vec![key("Property", 72), text_opt("Value", 64)], vec![vec![s("LAUNCHAPP"), s("1")]])?;
    Ok(())
}
