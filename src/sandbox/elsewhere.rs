//! The sandbox off Linux, where Landlock is not: nothing to apply, and the posture says so — so
//! the engine and every file loader refuse to go on unless the operator starts benchy with
//! `--allow-unsandboxed`.

use super::Posture;
use std::path::Path;

pub fn landlock_abi() -> u32 {
    0
}

pub fn restrict_to(_path: &Path, _session: &Path) -> Posture {
    Posture::Unavailable("Landlock is Linux-only, and nothing equivalent is wired up here".into())
}

pub fn restrict_loader(_input: &Path, _stage: &Path) -> Posture {
    Posture::Unavailable("Landlock is Linux-only, and nothing equivalent is wired up here".into())
}

pub fn restrict_network_loader(_stage: &Path, _ports: &[u16]) -> Posture {
    Posture::Unavailable("Landlock is Linux-only, and nothing equivalent is wired up here".into())
}
