//! The device table.
//!
//! SANE's pixma backend derives behaviour from PID comparisons scattered
//! through the code (protocol generation from PID ordering, button layout
//! and pixel reordering from PID lists). Here each of those is a field, so a
//! model's behaviour is visible in one place.
//!
//! Only models whose behaviour has been checked against this port are
//! listed. Others from `pixma_mp150.c` can be added once a recording or the
//! hardware confirms them.

/// Canon's USB vendor ID.
pub const CANON_VID: u16 = 0x04a9;

/// How a model reports its front-panel buttons on the interrupt endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buttons {
    /// Target in byte `0x13`; `finish` is button 2, everything else button 1
    /// (`LiDE` 300 and 400).
    Target13 { finish: u8 },
}

/// One supported scanner model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// Marketing name, e.g. `CanoScan LiDE 400`.
    pub name: &'static str,
    pub pid: u16,
    /// Lowest resolution the hardware scans at; lower ones are produced by
    /// scanning at this and shrinking in software.
    pub min_xdpi: u32,
    /// Highest horizontal resolution.
    pub xdpi: u32,
    /// Highest vertical resolution.
    pub ydpi: u32,
    /// Bed width in 1/75 inch.
    pub width: u32,
    /// Bed height in 1/75 inch.
    pub height: u32,
    /// 48-bit colour and 16-bit gray modes.
    pub high_depth: bool,
    /// mp150 protocol generation (1 to 5).
    pub generation: u8,
    pub buttons: Buttons,
}

/// Every supported model.
pub const MODELS: &[Model] = &[
    Model {
        name: "CanoScan LiDE 400",
        pid: 0x1912,
        min_xdpi: 300,
        xdpi: 4800,
        ydpi: 9600,
        width: 638,
        height: 877,
        high_depth: true,
        generation: 5,
        buttons: Buttons::Target13 { finish: 0x06 },
    },
    Model {
        name: "CanoScan LiDE 300",
        pid: 0x1913,
        min_xdpi: 300,
        xdpi: 2400,
        ydpi: 4800,
        width: 638,
        height: 877,
        high_depth: false,
        generation: 5,
        buttons: Buttons::Target13 { finish: 0x03 },
    },
];

/// The model with USB IDs `vid`:`pid`, if supported.
#[must_use]
pub fn find(vid: u16, pid: u16) -> Option<&'static Model> {
    if vid != CANON_VID {
        return None;
    }
    MODELS.iter().find(|m| m.pid == pid)
}
