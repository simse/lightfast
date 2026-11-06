//! The list of compiled-in backends.

use scan_core::driver::NetDriver;
use scan_usb::UsbDriver;

pub(crate) const USB: &[UsbDriver] = &[
    scan_pixma::DRIVER,
    scan_hpljm1005::DRIVER,
    scan_ricoh2::DRIVER,
    scan_lexmark_x2600::DRIVER,
    scan_cardscan::DRIVER,
    scan_ma1509::DRIVER,
    scan_xerox_mfp::DRIVER,
];

pub(crate) const NET: &[NetDriver] = &[scan_dell1600n_net::DRIVER, scan_xerox_mfp::TCP_DRIVER];
