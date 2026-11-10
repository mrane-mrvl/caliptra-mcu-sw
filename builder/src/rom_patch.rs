// Licensed under the Apache-2.0 license

//! Host-side MCU ROM patch builder, and the checks every patch-capable ROM
//! build runs on its ELF.
//!
//! The builder does not diff ROM images. It rebuilds the Instruction RAM and
//! Data RAM contents each ROM build produces at boot, from the ELF sections
//! whose VMA falls in those windows, and diffs those. Every offset it emits is
//! therefore already in the offset space the patch operations use. The blob is
//! encoded with the shared `caliptra-mcu-rom-patch` crate and checked by
//! running that crate's `apply`, the same code the ROM runs.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use anyhow::{anyhow, bail, ensure, Context, Result};
use caliptra_mcu_config_emulator::{ROM_PATCH_BLOB_SIZE, ROM_PATCH_REGION_START};
use caliptra_mcu_rom_patch::{
    apply, end_header, op_header, start_header, MAX_OP_DATA_WORDS, OP_OVERWRITE_IRAM, PATCH_OK,
    WORD_BYTES,
};
use elf::abi::{
    PT_LOAD, R_RISCV_ALIGN, R_RISCV_NONE, R_RISCV_PCREL_LO12_I, R_RISCV_PCREL_LO12_S,
    R_RISCV_RELAX, SHF_ALLOC, SHT_NOBITS, SHT_RELA,
};
use elf::endian::AnyEndian;
use elf::ElfBytes;

/// Every loaded (`SHF_ALLOC`) section `emulator-rom-patching-layout.ld` places.
/// This is a second copy of the layout's list: change both together.
const LAYOUT_SECTIONS: &[&str] = &[
    ".text.nonpatchable",
    ".rodata.nonpatchable",
    ".text",
    ".data",
    ".bss",
    ".stack",
    ".estack",
    ".patch_funcs",
];

/// Sections that stay in ROM and are never copied to SRAM, so no patch
/// operation can change them.
const ROM_ONLY_SECTIONS: &[&str] = &[".text.nonpatchable", ".rodata.nonpatchable"];

const PATCH_FUNCS_SECTION: &str = ".patch_funcs";

/// The only Instruction RAM symbols non-patchable code may reference. The
/// first two are the intended jumps into patchable code: the branch to the
/// patched copy and the `mtvec` restore. The rest are linker-script labels
/// whose addresses are baked into the referencing instruction; `boot.rs` uses
/// them as copy and patch destinations and never reads through them.
const NON_PATCHABLE_IRAM_REFS: &[&str] = &[
    "patchable_entry",
    "exception_handler",
    "PATCHABLE_TEXT_START",
    "PATCHABLE_TEXT_END",
    "PATCH_IRAM_START",
    "PATCH_IRAM_END",
];

/// Shift guard differences shown in one error.
const MAX_REPORTED_SHIFTS: usize = 20;

/// A patch blob and the number of operations in it.
pub struct RomPatch {
    pub blob: Vec<u8>,
    pub op_count: usize,
}

/// Builds the patch that turns the `baseline` ROM's Instruction RAM into the
/// `modified` ROM's. Both ELFs come from `rom-build --features rom-patching`.
pub fn rom_patch_build(baseline: &Path, modified: &Path) -> Result<RomPatch> {
    let baseline = RomElf::read(baseline)?;
    let modified = RomElf::read(modified)?;
    check_rom_elf(&baseline).context("baseline ROM")?;
    check_rom_elf(&modified).context("modified ROM")?;
    build_patch(&baseline, &modified)
}

/// Fails if a patch-capable ROM ELF places a loaded section outside the
/// patching layout, or if its non-patchable code or data references
/// Instruction RAM other than through [`NON_PATCHABLE_IRAM_REFS`]. Run right
/// after linking: once the ROM is taped out, neither can be fixed.
pub(crate) fn check_patchable_rom_elf(path: &Path) -> Result<()> {
    check_rom_elf(&RomElf::read(path)?)
}

/// An `SHF_ALLOC` section. `data` is empty for `SHT_NOBITS` sections.
#[derive(Debug, Default)]
struct Section {
    name: String,
    vma: u64,
    /// `None` for an empty section, which no load segment covers.
    lma: Option<u64>,
    size: u64,
    data: Vec<u8>,
}

#[derive(Debug)]
struct Symbol {
    name: String,
    addr: u64,
    size: u64,
}

/// A relocation the linker kept (`--emit-relocs`) in a ROM-only section.
#[derive(Debug)]
struct Reloc {
    section: String,
    /// Address of the relocated instruction or data.
    addr: u64,
    r_type: u32,
    symbol: String,
    addend: i64,
    /// Address the relocation refers to: symbol value plus addend.
    target: u64,
}

/// The parts of a ROM ELF the builder and the checks use.
#[derive(Debug, Default)]
struct RomElf {
    sections: Vec<Section>,
    symbols: Vec<Symbol>,
    relocs: Vec<Reloc>,
}

impl RomElf {
    fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let elf = ElfBytes::<AnyEndian>::minimal_parse(bytes)?;
        let (Some(shdrs), Some(shstrtab)) = elf.section_headers_with_strtab()? else {
            bail!("no section headers");
        };
        let (symtab, strtab) = elf
            .symbol_table()?
            .ok_or_else(|| anyhow!("no symbol table"))?;
        let segments: Vec<_> = elf
            .segments()
            .map(|s| s.iter().filter(|p| p.p_type == PT_LOAD).collect())
            .unwrap_or_default();

        let mut rom = RomElf::default();
        for shdr in shdrs.iter() {
            if shdr.sh_flags & u64::from(SHF_ALLOC) == 0 {
                continue;
            }
            let (vma, size) = (shdr.sh_addr, shdr.sh_size);
            let lma = segments
                .iter()
                .find(|p| size > 0 && vma >= p.p_vaddr && vma + size <= p.p_vaddr + p.p_memsz)
                .map(|p| vma - p.p_vaddr + p.p_paddr);
            let data = if shdr.sh_type == SHT_NOBITS {
                Vec::new()
            } else {
                elf.section_data(&shdr)?.0.to_vec()
            };
            rom.sections.push(Section {
                name: shstrtab.get(shdr.sh_name as usize)?.to_string(),
                vma,
                lma,
                size,
                data,
            });
        }

        // Indexed like the symbol table, for relocation lookups.
        let all_symbols: Vec<(String, u64)> = symtab
            .iter()
            .map(|s| Ok((strtab.get(s.st_name as usize)?.to_string(), s.st_value)))
            .collect::<Result<_>>()?;
        rom.symbols = symtab
            .iter()
            .zip(&all_symbols)
            // `$x`/`$d` are RISC-V mapping symbols: they mark code and data
            // runs, carry no size, and say nothing about what moved.
            .filter(|(s, (name, _))| {
                !s.is_undefined() && !name.is_empty() && !name.starts_with('$')
            })
            .map(|(s, (name, _))| Symbol {
                name: name.clone(),
                addr: s.st_value,
                size: s.st_size,
            })
            .collect();

        for shdr in shdrs.iter().filter(|s| s.sh_type == SHT_RELA) {
            let target = shdrs.get(shdr.sh_info as usize)?;
            let section = shstrtab.get(target.sh_name as usize)?;
            if !ROM_ONLY_SECTIONS.contains(&section) {
                continue;
            }
            for rela in elf.section_data_as_relas(&shdr)? {
                let (symbol, value) = all_symbols
                    .get(rela.r_sym as usize)
                    .ok_or_else(|| anyhow!("relocation symbol {} out of range", rela.r_sym))?;
                rom.relocs.push(Reloc {
                    section: section.to_string(),
                    addr: rela.r_offset,
                    r_type: rela.r_type,
                    symbol: symbol.clone(),
                    addend: rela.r_addend,
                    target: value.wrapping_add_signed(rela.r_addend),
                });
            }
        }
        Ok(rom)
    }

    fn section(&self, name: &str) -> Result<&Section> {
        self.sections
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow!("no {name} section"))
    }

    /// Address of the one symbol named `name`.
    fn symbol(&self, name: &str) -> Result<u64> {
        let mut found = self.symbols.iter().filter(|s| s.name == name);
        match (found.next(), found.next()) {
            (Some(s), None) => Ok(s.addr),
            (None, _) => bail!("no {name} symbol"),
            (Some(_), Some(_)) => bail!("more than one {name} symbol"),
        }
    }

    /// The window from symbol `start` to symbol `end`.
    fn window(&self, start: &str, end: &str) -> Result<Range<u64>> {
        let window = self.symbol(start)?..self.symbol(end)?;
        ensure!(
            window.start <= window.end && window.start % 4 == 0 && window.end % 4 == 0,
            "{start}..{end} ({:#x}..{:#x}) is not a word-aligned window",
            window.start,
            window.end
        );
        Ok(window)
    }

    /// The contents of `window` at boot: every section whose VMA falls in it,
    /// copied to `ram[vma - window.start]`. Everything else stays zero.
    fn ram_image(&self, window: &Range<u64>) -> Result<Vec<u8>> {
        let mut ram = vec![0; (window.end - window.start) as usize];
        for s in self.sections.iter().filter(|s| !s.data.is_empty()) {
            let end = s.vma + s.size;
            if s.vma >= window.start && end <= window.end {
                let offset = (s.vma - window.start) as usize;
                ram[offset..offset + s.data.len()].copy_from_slice(&s.data);
            } else if s.vma < window.end && end > window.start {
                bail!(
                    "{} ({:#x}..{end:#x}) straddles RAM window {:#x}..{:#x}",
                    s.name,
                    s.vma,
                    window.start,
                    window.end
                );
            }
        }
        Ok(ram)
    }

    /// Name of the sized symbol that contains `addr`, for error messages.
    fn symbol_at(&self, addr: u64) -> &str {
        self.symbols
            .iter()
            .find(|s| s.size > 0 && (s.addr..s.addr + s.size).contains(&addr))
            .map_or("?", |s| s.name.as_str())
    }
}

fn check_rom_elf(elf: &RomElf) -> Result<()> {
    check_loaded_sections(elf)?;
    check_non_patchable_refs(elf)
}

/// Fails if any loaded section is not one the patching layout places. An
/// orphan section the linker places on its own can land outside the
/// patchable copy without any link error.
fn check_loaded_sections(elf: &RomElf) -> Result<()> {
    let orphans: Vec<String> = elf
        .sections
        .iter()
        .filter(|s| !LAYOUT_SECTIONS.contains(&s.name.as_str()))
        .map(|s| format!("{} at {:#x} ({} B)", s.name, s.vma, s.size))
        .collect();
    ensure!(
        orphans.is_empty(),
        "loaded sections outside the ROM patching layout: {}. Place them in \
         firmware-bundler/data/emulator-rom-patching-layout.ld and add them to \
         LAYOUT_SECTIONS in builder/src/rom_patch.rs",
        orphans.join(", ")
    );
    Ok(())
}

/// Fails if non-patchable code or data uses a patchable address, other than
/// the ones in [`NON_PATCHABLE_IRAM_REFS`]. Non-patchable code must not depend
/// on patchable code or data: a patch could change them.
///
/// This check reads the linker's relocation records, which `--emit-relocs`
/// keeps in the ELF. A relocation record says "this instruction uses the
/// address of symbol X". The check has two blind spots:
/// - An address written as a plain number in the source has no relocation
///   record, so the check does not see it.
/// - An allowed symbol always passes, whether the code writes to it or reads
///   from it.
fn check_non_patchable_refs(elf: &RomElf) -> Result<()> {
    ensure!(
        !elf.relocs.is_empty(),
        "no relocation records for the ROM-only sections; link the ROM with --emit-relocs"
    );
    let iram = elf.window("PATCH_IRAM_START", "PATCH_IRAM_END")?;
    let bad: Vec<String> = elf
        .relocs
        .iter()
        // These carry no target of their own: a PCREL_LO12 points back at its
        // `auipc`, whose PCREL_HI20 holds the real target.
        .filter(|r| {
            ![
                R_RISCV_NONE,
                R_RISCV_RELAX,
                R_RISCV_ALIGN,
                R_RISCV_PCREL_LO12_I,
                R_RISCV_PCREL_LO12_S,
            ]
            .contains(&r.r_type)
        })
        .filter(|r| iram.contains(&r.target))
        .filter(|r| !(NON_PATCHABLE_IRAM_REFS.contains(&r.symbol.as_str()) && r.addend == 0))
        .map(|r| {
            format!(
                "{} {:#x} (in {}) -> {}{:+} ({:#x})",
                r.section,
                r.addr,
                elf.symbol_at(r.addr),
                r.symbol,
                r.addend,
                r.target
            )
        })
        .collect();
    ensure!(
        bad.is_empty(),
        "non-patchable code or data refers to patchable Instruction RAM:\n  {}\n\
         Non-patchable code must not call patchable code or read patchable data. \
         The only allowed references are {NON_PATCHABLE_IRAM_REFS:?}",
        bad.join("\n  ")
    );
    Ok(())
}

fn build_patch(baseline: &RomElf, modified: &RomElf) -> Result<RomPatch> {
    // Shift guard first: a shift also changes the address immediates in
    // ROM-only code, and the guard names the symbol that moved.
    check_no_shift(baseline, modified)?;
    check_rom_only_sections_match(baseline, modified)?;

    // The shift guard has already shown both ELFs agree on these symbols.
    let iram = baseline.window("PATCH_IRAM_START", "PATCH_IRAM_END")?;
    ensure!(
        iram.start == u64::from(ROM_PATCH_REGION_START),
        "PATCH_IRAM_START is {:#x}, but ROM_PATCH_REGION_START is {ROM_PATCH_REGION_START:#x}",
        iram.start
    );
    let dram = baseline.window("DATA_START", "DATA_END")?;
    let base_iram = baseline.ram_image(&iram)?;
    let mod_iram = modified.ram_image(&iram)?;
    let base_dram = baseline.ram_image(&dram)?;
    let mod_dram = modified.ram_image(&dram)?;

    // PoC only: remove once the builder emits Overwrite Data RAM, and once
    // Data RAM patches survive the warm reset (`start.s` re-copies `.data`).
    if let Some(offset) = first_difference(&base_dram, &mod_dram) {
        bail!(
            "Data RAM differs at offset {offset:#x}; the builder does not emit \
             Overwrite Data RAM yet"
        );
    }
    // PoC only: remove once the builder emits Write Patch Function, and then
    // exclude `.patch_funcs` from the diff below, since that op carries its
    // bytes.
    if let Some(offset) = first_difference(
        &baseline.section(PATCH_FUNCS_SECTION)?.data,
        &modified.section(PATCH_FUNCS_SECTION)?.data,
    ) {
        bail!(
            "{PATCH_FUNCS_SECTION} differs at offset {offset:#x}; the builder does not \
             emit Write Patch Function yet"
        );
    }

    let ranges = differing_word_ranges(&base_iram, &mod_iram);
    ensure!(
        !ranges.is_empty(),
        "no differences in Instruction RAM; nothing to patch"
    );
    let blob = encode(&mod_iram, &ranges)?;

    // Self-check: the shared `apply` must turn the baseline into the modified
    // image exactly.
    let (mut patched_iram, mut patched_dram) = (base_iram, base_dram);
    let code = apply(&blob, &mut patched_iram, &mut patched_dram);
    ensure!(code == PATCH_OK, "self-check: apply returned {code:#x}");
    if let Some(offset) = first_difference(&patched_iram, &mod_iram) {
        bail!("self-check: patched Instruction RAM differs at offset {offset:#x}");
    }
    if let Some(offset) = first_difference(&patched_dram, &mod_dram) {
        bail!("self-check: patched Data RAM differs at offset {offset:#x}");
    }
    Ok(RomPatch {
        blob,
        op_count: ranges.len(),
    })
}

/// Fails if a ROM-only section differs: no patch operation can reach it.
fn check_rom_only_sections_match(baseline: &RomElf, modified: &RomElf) -> Result<()> {
    for name in ROM_ONLY_SECTIONS {
        let (base, modi) = (baseline.section(name)?, modified.section(name)?);
        if let Some(offset) = first_difference(&base.data, &modi.data) {
            bail!(
                "{name} differs at {:#x}; ROM-only code and data cannot be patched",
                base.vma + offset as u64
            );
        }
    }
    Ok(())
}

/// The shift guard: every loaded section keeps its VMA, LMA and size, and
/// every symbol keeps its address and size. A patch overwrites bytes in place,
/// so nothing that moves code can be patched.
fn check_no_shift(baseline: &RomElf, modified: &RomElf) -> Result<()> {
    let mut shifts = Vec::new();

    let sections = |elf: &RomElf| -> BTreeMap<String, (u64, Option<u64>, u64)> {
        elf.sections
            .iter()
            .map(|s| (s.name.clone(), (s.vma, s.lma, s.size)))
            .collect()
    };
    let show = |s: &Option<&(u64, Option<u64>, u64)>| match s {
        Some((vma, Some(lma), size)) => format!("vma {vma:#x} lma {lma:#x} size {size:#x}"),
        Some((vma, None, size)) => format!("vma {vma:#x} size {size:#x}"),
        None => "missing".to_string(),
    };
    let (base, modi) = (sections(baseline), sections(modified));
    for name in base
        .keys()
        .chain(modi.keys().filter(|n| !base.contains_key(*n)))
    {
        let (b, m) = (base.get(name), modi.get(name));
        if b != m {
            shifts.push(format!("section {name}: {} -> {}", show(&b), show(&m)));
        }
    }

    // Symbol names can repeat (local symbols), so compare every
    // address/size pair under a name.
    let symbols = |elf: &RomElf| -> BTreeMap<String, Vec<(u64, u64)>> {
        let mut map: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
        for s in &elf.symbols {
            map.entry(s.name.clone())
                .or_default()
                .push((s.addr, s.size));
        }
        map.values_mut().for_each(|v| v.sort());
        map
    };
    let show = |s: Option<&Vec<(u64, u64)>>| match s {
        Some(v) => v
            .iter()
            .map(|(addr, size)| format!("{addr:#x}+{size:#x}"))
            .collect::<Vec<_>>()
            .join(", "),
        None => "missing".to_string(),
    };
    let (base, modi) = (symbols(baseline), symbols(modified));
    let mut moved = Vec::new();
    for name in base
        .keys()
        .chain(modi.keys().filter(|n| !base.contains_key(*n)))
    {
        let (b, m) = (base.get(name), modi.get(name));
        if b != m {
            let addr = b.or(m).map_or(0, |v| v[0].0);
            moved.push((addr, format!("symbol {name}: {} -> {}", show(b), show(m))));
        }
    }
    // Lowest address first: the first symbol listed is the one that changed
    // size or appeared, and the rest moved because of it.
    moved.sort();
    shifts.extend(moved.into_iter().map(|(_, line)| line));

    ensure!(
        shifts.is_empty(),
        "the modified ROM moves code or data, so it cannot be patched in place. \
         {} difference(s) (baseline -> modified), first {MAX_REPORTED_SHIFTS}:\n  {}",
        shifts.len(),
        shifts
            .iter()
            .take(MAX_REPORTED_SHIFTS)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
    Ok(())
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len() != b.len() {
        return Some(a.len().min(b.len()));
    }
    a.iter().zip(b).position(|(x, y)| x != y)
}

/// The word ranges where `base` and `modified` differ. Each range is a
/// stretch of adjacent differing words and becomes one operation; an
/// unchanged word in between starts a new range.
fn differing_word_ranges(base: &[u8], modified: &[u8]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let words = base
        .chunks_exact(WORD_BYTES)
        .zip(modified.chunks_exact(WORD_BYTES));
    for (word, (b, m)) in words.enumerate() {
        if b == m {
            continue;
        }
        match ranges.last_mut() {
            Some(range) if range.end == word => range.end += 1,
            _ => ranges.push(word..word + 1),
        }
    }
    ranges
}

/// Encodes one Overwrite Instruction RAM operation per range, with the
/// range's words taken from `modified`.
fn encode(modified: &[u8], ranges: &[Range<usize>]) -> Result<Vec<u8>> {
    let words = 2 + ranges.iter().map(|r| 1 + r.len()).sum::<usize>();
    let budget = ROM_PATCH_BLOB_SIZE as usize;
    ensure!(
        words * WORD_BYTES <= budget,
        "patch is {} B ({} operation(s)); the blob budget is {budget} B",
        words * WORD_BYTES,
        ranges.len()
    );

    let mut blob = Vec::with_capacity(words * WORD_BYTES);
    blob.extend(start_header(u16::try_from(words)?));
    for range in ranges {
        // Unreachable while the budget is 1 KiB, which caps an operation at
        // 253 words; this guards a larger budget.
        let data_words = u8::try_from(range.len()).map_err(|_| {
            anyhow!(
                "operation at word {:#x} is {} words; one operation holds at most \
                 {MAX_OP_DATA_WORDS}",
                range.start,
                range.len()
            )
        })?;
        let offset = u16::try_from(range.start)
            .map_err(|_| anyhow!("word offset {:#x} does not fit an operation", range.start))?;
        blob.extend(op_header(OP_OVERWRITE_IRAM, data_words, offset));
        blob.extend_from_slice(&modified[range.start * WORD_BYTES..range.end * WORD_BYTES]);
    }
    blob.extend(end_header());
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use caliptra_mcu_rom_patch::OP_START;

    const IRAM_START: u64 = ROM_PATCH_REGION_START as u64;
    const TEXT_START: u64 = IRAM_START + 0x400;
    const IRAM_END: u64 = TEXT_START + 0x100;
    const DATA_START: u64 = 0x5000_3c00;

    fn section(name: &str, vma: u64, lma: u64, data: Vec<u8>) -> Section {
        Section {
            name: name.to_string(),
            vma,
            lma: (!data.is_empty()).then_some(lma),
            size: data.len() as u64,
            data,
        }
    }

    fn symbol(name: &str, addr: u64, size: u64) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
        }
    }

    fn reloc(symbol: &str, addend: i64, target: u64) -> Reloc {
        Reloc {
            section: ".text.nonpatchable".to_string(),
            addr: 0x8000_0010,
            r_type: elf::abi::R_RISCV_HI20,
            symbol: symbol.to_string(),
            addend,
            target,
        }
    }

    /// A ROM with a 0x100-byte patchable `.text` filled with `fill`, empty
    /// `.data` and `.patch_funcs`, and one non-patchable call to
    /// `patchable_entry`.
    fn rom(fill: u8) -> RomElf {
        RomElf {
            sections: vec![
                section(
                    ".text.nonpatchable",
                    0x8000_0000,
                    0x8000_0000,
                    vec![0x13; 0x20],
                ),
                section(
                    ".rodata.nonpatchable",
                    0x8000_0020,
                    0x8000_0020,
                    vec![1, 2, 3, 4],
                ),
                section(".text", TEXT_START, 0x8000_0024, vec![fill; 0x100]),
                section(".data", DATA_START, 0x8000_0124, vec![]),
                section(".patch_funcs", IRAM_START, 0x8000_0124, vec![]),
            ],
            symbols: vec![
                symbol("PATCH_IRAM_START", IRAM_START, 0),
                symbol("PATCH_IRAM_END", IRAM_END, 0),
                symbol("DATA_START", DATA_START, 0),
                symbol("DATA_END", DATA_START, 0),
                symbol("start", 0x8000_0000, 0x20),
                symbol("patchable_entry", TEXT_START, 0x10),
                symbol("foo", TEXT_START + 0x10, 0x20),
            ],
            relocs: vec![reloc("patchable_entry", 0, TEXT_START)],
        }
    }

    fn text_mut(elf: &mut RomElf) -> &mut Vec<u8> {
        &mut elf
            .sections
            .iter_mut()
            .find(|s| s.name == ".text")
            .unwrap()
            .data
    }

    fn err(result: Result<impl Sized>) -> String {
        format!("{:#}", result.err().expect("expected an error"))
    }

    #[test]
    fn one_word_change_is_one_operation() {
        let base = rom(0);
        let mut modi = rom(0);
        text_mut(&mut modi)[0x11] = 0x50; // word 0x4 of .text, byte 1

        let patch = build_patch(&base, &modi).unwrap();

        // Instruction RAM word offset: 0x400 B of .patch_funcs + 0x10 B.
        let offset = ((0x400 + 0x10) / 4) as u16;
        let mut expected = start_header(4).to_vec();
        expected.extend(op_header(OP_OVERWRITE_IRAM, 1, offset));
        expected.extend([0, 0x50, 0, 0]);
        expected.extend(end_header());
        assert_eq!(patch.blob, expected);
        assert_eq!(patch.op_count, 1);
    }

    #[test]
    fn only_adjacent_words_coalesce() {
        let base = rom(0);
        let mut modi = rom(0);
        let text = text_mut(&mut modi);
        text[0] = 1; // word 0
        text[4] = 1; // word 1, adjacent: same operation
        text[12] = 1; // word 3, after an unchanged word: new operation

        let patch = build_patch(&base, &modi).unwrap();

        assert_eq!(patch.op_count, 2);
        // Start + (header + 2 words) + (header + 1 word) + End.
        assert_eq!(patch.blob.len(), 7 * WORD_BYTES);
        assert_eq!(patch.blob[0], OP_START);
    }

    #[test]
    fn rom_only_change_is_rejected() {
        let mut modi = rom(0);
        modi.sections[1].data[2] = 9;
        text_mut(&mut modi)[0] = 1;
        let e = err(build_patch(&rom(0), &modi));
        assert!(
            e.contains(".rodata.nonpatchable differs at 0x80000022"),
            "{e}"
        );
    }

    #[test]
    fn moved_symbol_is_named() {
        let mut modi = rom(0);
        modi.symbols[6].addr += 4;
        text_mut(&mut modi)[0] = 1;
        let e = err(build_patch(&rom(0), &modi));
        assert!(
            e.contains("symbol foo: 0x40080410+0x20 -> 0x40080414+0x20"),
            "{e}"
        );
    }

    #[test]
    fn orphan_loaded_section_is_rejected() {
        let mut elf = rom(0);
        elf.sections
            .push(section(".srodata.x", 0x8000_0200, 0x8000_0200, vec![0; 4]));
        let e = err(check_rom_elf(&elf));
        assert!(e.contains(".srodata.x at 0x80000200 (4 B)"), "{e}");
    }

    #[test]
    fn patchable_reference_is_rejected() {
        let mut elf = rom(0);
        elf.relocs.push(reloc("foo", 0, TEXT_START + 0x10));
        let e = err(check_rom_elf(&elf));
        assert!(e.contains("(in start) -> foo+0 (0x40080410)"), "{e}");
    }
}
