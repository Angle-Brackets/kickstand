// SPDX-License-Identifier: GPL-3.0-only
//! Horizon services nx doesn't wrap. Command ids and layouts follow libnx (ISC); each wrapper names its
//! source. Only firmware 5.0+ ids are used.

#![no_std]

extern crate alloc;

pub mod audio;
pub mod caps;
pub mod debug;
pub mod grc;
pub mod hid;
pub mod idle;
pub mod ns;
pub mod pl;
pub mod pm;
pub mod power;
pub mod set;
pub mod usbds;
