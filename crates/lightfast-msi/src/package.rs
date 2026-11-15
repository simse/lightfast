//! The installer database: what to install where, and how to upgrade.
//!
//! The package installs per machine into `Program Files\Lightfast`, with a
//! Start menu shortcut. Every build is a major upgrade: it has a new product
//! code and removes any installed Lightfast (found by [`UPGRADE_CODE`])
//! before installing its own files, so updates never leave files behind and
//! a build can replace another of the same version. Downgrades are allowed,
//! like reinstalling an older `AppImage` or DMG.

use std::fs::{self, File};
use std::io::{self, Cursor, Seek, Write};
use std::path::Path;

use anyhow::{Context as _, bail, ensure};
use msi::{Category, CodePage, Column, Insert, LanguageId, Package, PackageType, Value};
use uuid::Uuid;

use crate::{Input, ui};

/// Identifies Lightfast across every version and architecture. Never change
/// it: installed copies find their upgrades by it.
pub const UPGRADE_CODE: Uuid = uuid::uuid!("24EA6BE3-2385-4B0B-96A5-089784EA5610");

/// US English, the only language the installer has.
const LANGUAGE: u16 = 1033;

/// The Add or Remove Programs publisher.
const MANUFACTURER: &str = "simse";

/// The cabinet holding every file, stored in the package as a stream.
const CABINET: &str = "lightfast.cab";

/// The icon shown in Add or Remove Programs.
const PRODUCT_ICON: &str = "ProductIcon.ico";

/// Component attributes: the component is 64-bit.
const COMPONENT_64BIT: i32 = 256;
/// File attributes: the file is vital, so failing to install it fails the
/// install.
const FILE_VITAL: i32 = 512;
/// Upgrade attributes: `VersionMin` is inclusive.
const UPGRADE_VERSION_MIN_INCLUSIVE: i32 = 256;
/// Summary Information word count: long file names, compressed files.
const WORD_COUNT_COMPRESSED: i32 = 2;

/// A file the package installs into the install directory.
struct InstallFile<'a> {
    /// The `File` and `Component` key.
    key: &'static str,
    /// `short|long`, as the `FileName` column wants.
    name: &'static str,
    source: &'a Path,
}

/// Fails unless `version` is the `major.minor.patch` Windows Installer
/// requires, with fields in its ranges.
pub fn check_version(version: &str) -> anyhow::Result<()> {
    let fields: Vec<&str> = version.split('.').collect();
    ensure!(fields.len() == 3, "version {version:?} isn't major.minor.patch");
    for (field, max) in fields.iter().zip([255, 255, 65535]) {
        let n: u32 = field.parse().with_context(|| format!("version {version:?} has a non-numeric field"))?;
        ensure!(n <= max, "version {version:?} has a field above Windows Installer's limit of {max}");
    }
    Ok(())
}

/// A GUID derived from the upgrade code and `seed`: the same seed always
/// gives the same GUID, as component GUIDs must across versions.
fn guid(seed: &str) -> String {
    format!("{{{}}}", Uuid::new_v5(&UPGRADE_CODE, seed.as_bytes()).hyphenated()).to_uppercase()
}

/// A string cell. Windows Installer has no empty strings, only null: the
/// msi crate would store `""` as a zero-length string-pool entry, which
/// Windows reads as the start of a long string and then reports the pool as
/// corrupted.
pub fn s(value: &str) -> Value {
    if value.is_empty() { Value::Null } else { Value::Str(value.to_owned()) }
}

pub fn n(value: i32) -> Value {
    Value::Int(value)
}

/// A primary-key identifier column.
pub fn key(name: &str, len: usize) -> Column {
    Column::build(name).primary_key().id_string(len)
}

/// A required identifier column (a foreign key, by convention).
pub fn id(name: &str, len: usize) -> Column {
    Column::build(name).id_string(len)
}

/// An optional identifier column.
pub fn id_opt(name: &str, len: usize) -> Column {
    Column::build(name).nullable().id_string(len)
}

/// A required formatted or plain text column.
pub fn text(name: &str, len: usize) -> Column {
    Column::build(name).text_string(len)
}

/// An optional text column.
pub fn text_opt(name: &str, len: usize) -> Column {
    Column::build(name).nullable().text_string(len)
}

/// An optional, localizable text column.
pub fn loc_opt(name: &str, len: usize) -> Column {
    Column::build(name).nullable().localizable().text_string(len)
}

/// A required 16-bit integer column.
pub fn i16_(name: &str) -> Column {
    Column::build(name).int16()
}

/// An optional 16-bit integer column.
pub fn i16_opt(name: &str) -> Column {
    Column::build(name).nullable().int16()
}

/// An optional 32-bit integer column.
pub fn i32_opt(name: &str) -> Column {
    Column::build(name).nullable().int32()
}

/// Creates a table and inserts its rows.
pub fn table<F: io::Read + Write + Seek>(
    package: &mut Package<F>,
    name: &str,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
) -> anyhow::Result<()> {
    package.create_table(name, columns).with_context(|| format!("couldn't create the {name} table"))?;
    if !rows.is_empty() {
        package
            .insert_rows(Insert::into(name).rows(rows))
            .with_context(|| format!("couldn't fill the {name} table"))?;
    }
    Ok(())
}

/// The standard actions both sequences start with, through costing.
const COSTING: [(&str, i32); 6] = [
    ("FindRelatedProducts", 25),
    ("AppSearch", 50),
    ("LaunchConditions", 100),
    ("CostInitialize", 800),
    ("FileCost", 900),
    ("CostFinalize", 1000),
];

/// Writes the package to `out`.
#[expect(clippy::too_many_lines, reason = "one table after another reads best in order")]
pub fn build(input: &Input, out: &Path) -> anyhow::Result<()> {
    let files = [
        InstallFile { key: "LightfastExe", name: "LIGHTF~1.EXE|Lightfast.exe", source: &input.exe },
        InstallFile { key: "WinSparkleDll", name: "WINSPA~1.DLL|WinSparkle.dll", source: &input.winsparkle },
    ];
    let icon = fs::read(&input.icon).with_context(|| format!("couldn't read {}", input.icon.display()))?;

    // The package is built in memory, then written in one go, so a failed
    // build never leaves a half-written MSI behind.
    let mut package = Package::create(PackageType::Installer, Cursor::new(Vec::new()))?;
    let arch = input.arch;
    // Unique to this exact package, as Windows Installer requires: the same
    // version and architecture can only be built from the same files.
    let product_code = guid(&format!("product/{}/{}/{}", input.version, arch.name(), digest(&files)?));
    let package_code = guid(&format!("package/{product_code}"));

    // The msi crate defaults to UTF-8 (65001), which Windows Installer
    // doesn't accept as a database code page: it refuses to open the
    // package (error 1620). Every string here is ASCII.
    package.set_database_codepage(CodePage::Windows1252);
    let summary = package.summary_info_mut();
    summary.set_codepage(CodePage::Windows1252);
    summary.set_title("Installation Database");
    summary.set_subject("Lightfast");
    summary.set_author(MANUFACTURER);
    summary.set_comments("Flatbed photo and art scanner.");
    summary.set_keywords(&["Installer".to_owned()]);
    summary.set_creating_application("lightfast-msi");
    summary.set_arch(arch.platform());
    summary.set_languages(&[LanguageId::from_id(LANGUAGE)]);
    summary.set_uuid(Uuid::parse_str(package_code.trim_matches(['{', '}']))?);
    summary.set_page_count(arch.schema());
    summary.set_word_count(WORD_COUNT_COMPRESSED);
    summary.set_creation_time_to_now();

    table(
        &mut package,
        "Property",
        vec![key("Property", 72), text("Value", 0)],
        [
            ("ProductCode", product_code.as_str()),
            ("ProductName", "Lightfast"),
            ("ProductVersion", input.version.as_str()),
            ("ProductLanguage", "1033"),
            ("Manufacturer", MANUFACTURER),
            ("UpgradeCode", &guid_string(UPGRADE_CODE)),
            // Per machine, which needs elevation: Windows asks once, when the
            // install starts.
            ("ALLUSERS", "1"),
            // An elevated install only passes public properties it's told
            // are safe from the client to the server.
            ("SecureCustomProperties", "OLDPRODUCTS"),
            ("ARPPRODUCTICON", PRODUCT_ICON),
            ("ARPURLINFOABOUT", "https://github.com/simse/lightfast"),
            ("ARPHELPLINK", "https://github.com/simse/lightfast/issues"),
            // There are no features to choose between.
            ("ARPNOMODIFY", "1"),
            ("ARPNOREPAIR", "1"),
            ("LAUNCHAPP", "1"),
            ("DefaultUIFont", ui::DEFAULT_FONT),
        ]
        .into_iter()
        .map(|(k, v)| vec![s(k), s(v)])
        .collect(),
    )?;

    table(
        &mut package,
        "Directory",
        vec![key("Directory", 72), id_opt("Directory_Parent", 72), text("DefaultDir", 255)],
        vec![
            vec![s("TARGETDIR"), Value::Null, s("SourceDir")],
            vec![s("ProgramFiles64Folder"), s("TARGETDIR"), s(".")],
            vec![s("INSTALLDIR"), s("ProgramFiles64Folder"), s("LIGHTF~1|Lightfast")],
            vec![s("ProgramMenuFolder"), s("TARGETDIR"), s(".")],
        ],
    )?;

    table(
        &mut package,
        "Component",
        vec![
            key("Component", 72),
            Column::build("ComponentId").nullable().category(Category::Guid).string(38),
            id("Directory_", 72),
            i16_("Attributes"),
            text_opt("Condition", 255),
            id_opt("KeyPath", 72),
        ],
        files
            .iter()
            .map(|file| {
                // Seeded by key and architecture only, never the version:
                // a component's GUID must not change while its files'
                // paths don't.
                let component_id = guid(&format!("component/{}/{}", file.key, arch.name()));
                vec![s(file.key), s(&component_id), s("INSTALLDIR"), n(COMPONENT_64BIT), Value::Null, s(file.key)]
            })
            .collect(),
    )?;

    // File sizes, and the cabinet's contents in sequence order.
    let mut file_rows = Vec::new();
    let mut cabinet = cab::CabinetBuilder::new();
    let folder = cabinet.add_folder(cab::CompressionType::MsZip);
    for (sequence, file) in (1..).zip(&files) {
        let size = fs::metadata(file.source).with_context(|| format!("couldn't read {}", file.source.display()))?.len();
        let size = i32::try_from(size).with_context(|| format!("{} is over 2 GB", file.source.display()))?;
        file_rows.push(vec![
            s(file.key),
            s(file.key),
            s(file.name),
            n(size),
            Value::Null,
            Value::Null,
            n(FILE_VITAL),
            n(sequence),
        ]);
        folder.add_file(file.key);
    }
    let file_count = i32::try_from(files.len())?;
    table(
        &mut package,
        "File",
        vec![
            key("File", 72),
            id("Component_", 72),
            text("FileName", 255),
            Column::build("FileSize").int32(),
            Column::build("Version").nullable().string(72),
            Column::build("Language").nullable().string(20),
            i16_opt("Attributes"),
            i16_("Sequence"),
        ],
        file_rows,
    )?;

    table(
        &mut package,
        "Feature",
        vec![
            key("Feature", 38),
            id_opt("Feature_Parent", 38),
            loc_opt("Title", 64),
            loc_opt("Description", 255),
            i16_opt("Display"),
            i16_("Level"),
            id_opt("Directory_", 72),
            i16_("Attributes"),
        ],
        vec![vec![
            s("Complete"),
            Value::Null,
            s("Lightfast"),
            s("The Lightfast application."),
            n(1),
            n(1),
            s("INSTALLDIR"),
            n(0),
        ]],
    )?;
    table(
        &mut package,
        "FeatureComponents",
        vec![key("Feature_", 38), key("Component_", 72)],
        files.iter().map(|file| vec![s("Complete"), s(file.key)]).collect(),
    )?;

    table(
        &mut package,
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            i16_("LastSequence"),
            loc_opt("DiskPrompt", 64),
            Column::build("Cabinet").nullable().string(255),
            Column::build("VolumeLabel").nullable().string(32),
            id_opt("Source", 72),
        ],
        vec![vec![n(1), n(file_count), Value::Null, s(&format!("#{CABINET}")), Value::Null, Value::Null]],
    )?;

    // Not advertised, so it simply starts the installed executable, whose
    // own icon it shows.
    table(
        &mut package,
        "Shortcut",
        vec![
            key("Shortcut", 72),
            id("Directory_", 72),
            Column::build("Name").localizable().text_string(128),
            id("Component_", 72),
            text("Target", 72),
            text_opt("Arguments", 255),
            loc_opt("Description", 255),
            i16_opt("Hotkey"),
            id_opt("Icon_", 72),
            i16_opt("IconIndex"),
            i16_opt("ShowCmd"),
            id_opt("WkDir", 72),
        ],
        vec![vec![
            s("StartMenuShortcut"),
            s("ProgramMenuFolder"),
            s("LIGHTF~1|Lightfast"),
            s("LightfastExe"),
            s("[#LightfastExe]"),
            Value::Null,
            s("Scan photos and art"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            s("INSTALLDIR"),
        ]],
    )?;

    table(
        &mut package,
        "Icon",
        vec![key("Name", 72), Column::build("Data").binary()],
        vec![vec![s(PRODUCT_ICON), Value::Binary]],
    )?;
    package.write_stream(&format!("Icon.{PRODUCT_ICON}"))?.write_all(&icon)?;

    // Every installed Lightfast, of any version, goes in OLDPRODUCTS, which
    // RemoveExistingProducts uninstalls.
    table(
        &mut package,
        "Upgrade",
        vec![
            Column::build("UpgradeCode").primary_key().category(Category::Guid).string(38),
            Column::build("VersionMin").primary_key().nullable().string(20),
            Column::build("VersionMax").primary_key().nullable().string(20),
            Column::build("Language").primary_key().nullable().string(255),
            Column::build("Attributes").primary_key().int32(),
            text_opt("Remove", 255),
            id("ActionProperty", 72),
        ],
        vec![vec![
            s(&guid_string(UPGRADE_CODE)),
            s("0.0.0"),
            Value::Null,
            Value::Null,
            n(UPGRADE_VERSION_MIN_INCLUSIVE),
            Value::Null,
            s("OLDPRODUCTS"),
        ]],
    )?;

    // Launches the installed app from the last dialog, as the user rather
    // than elevated, since the UI sequence runs in the user's process.
    // Type 18: run a file the package installs, plus 192: asynchronously,
    // without waiting for it.
    table(
        &mut package,
        "CustomAction",
        vec![key("Action", 72), i16_("Type"), id_opt("Source", 72), text_opt("Target", 255)],
        vec![vec![s(ui::LAUNCH_ACTION), n(18 + 192), s("LightfastExe"), Value::Null]],
    )?;

    let mut execute: Vec<(&str, Option<&str>, i32)> = COSTING.iter().map(|&(a, n)| (a, None, n)).collect();
    execute.extend([
        ("InstallValidate", None, 1400),
        // Right after validation, before anything is installed: the old
        // version goes entirely, then this one installs cleanly.
        ("RemoveExistingProducts", None, 1401),
        ("InstallInitialize", None, 1500),
        ("ProcessComponents", None, 1600),
        ("UnpublishFeatures", None, 1800),
        ("RemoveShortcuts", None, 3200),
        ("RemoveFiles", None, 3500),
        ("RemoveFolders", None, 3600),
        ("CreateFolders", None, 3700),
        ("InstallFiles", None, 4000),
        ("CreateShortcuts", None, 4500),
        ("RegisterUser", None, 6000),
        ("RegisterProduct", None, 6100),
        ("PublishFeatures", None, 6300),
        ("PublishProduct", None, 6400),
        ("InstallFinalize", None, 6600),
    ]);
    sequence(&mut package, "InstallExecuteSequence", &execute)?;

    let mut ui_sequence: Vec<(&str, Option<&str>, i32)> = COSTING.iter().map(|&(a, n)| (a, None, n)).collect();
    ui_sequence.extend(ui::SEQUENCE.iter().copied());
    ui_sequence.push(("ExecuteAction", None, 1300));
    sequence(&mut package, "InstallUISequence", &ui_sequence)?;

    ui::add(&mut package)?;

    // The cabinet goes in last, as a stream named by the Media table.
    let mut cabinet = cabinet.build(Cursor::new(Vec::new()))?;
    while let Some(mut writer) = cabinet.next_file()? {
        let name = writer.file_name().to_owned();
        let file = files.iter().find(|f| f.key == name).context("the cabinet named an unknown file")?;
        io::copy(&mut File::open(file.source)?, &mut writer)?;
    }
    let cabinet = cabinet.finish()?.into_inner();
    package.write_stream(CABINET)?.write_all(&cabinet)?;

    let bytes = compound_file_v3(&package.into_inner()?.into_inner())?;
    let part = out.with_extension("msi.part");
    fs::write(&part, bytes).with_context(|| format!("couldn't write {}", part.display()))?;
    fs::rename(&part, out)?;
    Ok(())
}

/// Rewrites a compound file as version 3, with 512-byte sectors.
///
/// The `msi` crate writes version 4 (4096-byte sectors), which msitools
/// reads but Windows Installer refuses to open (error 1620). Every MSI that
/// Microsoft's tools and `WiX` make is version 3.
fn compound_file_v3(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut source = cfb::CompoundFile::open(Cursor::new(bytes))?;
    let mut dest = cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new()))?;
    let entries: Vec<_> = source.walk().collect();
    for entry in entries {
        let path = entry.path();
        if entry.is_storage() {
            if path != Path::new("/") {
                dest.create_storage(path)?;
            }
            dest.set_storage_clsid(path, *entry.clsid())?;
        } else {
            io::copy(&mut source.open_stream(path)?, &mut dest.create_stream(path)?)?;
        }
        if entry.state_bits() != 0 {
            dest.set_state_bits(path, entry.state_bits())?;
        }
    }
    dest.flush()?;
    Ok(dest.into_inner().into_inner())
}

/// Adds a sequence table: actions with their conditions and order.
fn sequence<F: io::Read + Write + Seek>(
    package: &mut Package<F>,
    name: &str,
    actions: &[(&str, Option<&str>, i32)],
) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (action, ..) in actions {
        if !seen.insert(action) {
            bail!("{name} has {action} twice");
        }
    }
    table(
        package,
        name,
        vec![key("Action", 72), text_opt("Condition", 255), i16_opt("Sequence")],
        actions
            .iter()
            .map(|&(action, condition, order)| vec![s(action), condition.map_or(Value::Null, s), n(order)])
            .collect(),
    )
}

/// `{XXXXXXXX-...}`, the form Windows Installer stores GUIDs in.
fn guid_string(uuid: Uuid) -> String {
    format!("{{{}}}", uuid.hyphenated()).to_uppercase()
}

/// A cheap fingerprint of the files' contents, so different builds of one
/// version get different product codes.
fn digest(files: &[InstallFile<'_>]) -> anyhow::Result<String> {
    // FNV-1a: no cryptographic strength needed, only distinct builds
    // getting distinct seeds.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        let bytes = fs::read(file.source).with_context(|| format!("couldn't read {}", file.source.display()))?;
        for byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    Ok(format!("{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_must_be_three_fields_in_range() {
        assert!(check_version("0.1.0").is_ok());
        assert!(check_version("255.255.65535").is_ok());
        assert!(check_version("1.2").is_err());
        assert!(check_version("1.2.3.4").is_err());
        assert!(check_version("256.0.0").is_err());
        assert!(check_version("1.x.0").is_err());
    }

    #[test]
    fn component_guids_are_stable() {
        assert_eq!(guid("component/LightfastExe/x64"), guid("component/LightfastExe/x64"));
        assert_ne!(guid("component/LightfastExe/x64"), guid("component/LightfastExe/arm64"));
        assert_eq!(guid_string(UPGRADE_CODE), "{24EA6BE3-2385-4B0B-96A5-089784EA5610}");
    }
}
