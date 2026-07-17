// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use rand::{RngCore, SeedableRng, rngs::StdRng};
use std::{fs, path::Path};
pub fn fixture_export() -> std::io::Result<tempfile::TempDir> {
    let d = tempfile::tempdir()?;
    fs::create_dir(d.path().join("folder"))?;
    fs::write(d.path().join("example.txt"), b"quicKFS fixture contents")?;
    fs::write(d.path().join("folder/nested.txt"), b"nested")?;
    Ok(d)
}
pub fn deterministic_bytes(length: usize) -> Vec<u8> {
    let mut data = vec![0; length];
    StdRng::seed_from_u64(0x5155_4943).fill_bytes(&mut data);
    data
}
pub fn write_fixture(path: &Path, data: &[u8]) -> std::io::Result<()> {
    fs::write(path, data)
}
