//! Bounded ISOBMFF walking — zero unsafe, zero FFI: metadata
//! extraction (ICC, irot/imir, first-frame item, ispe) and the
//! colr-splice used to embed profiles. Everything here handles
//! attacker-supplied bytes; keep it free of unsafe so it stays
//! trivially fuzzable without linking the codecs.

/// Read one box header at `i` within `end`: returns
/// `(fourcc, payload_range, box_end)`. `None` on truncation or a size
/// that cannot frame its own header.
pub(super) fn next_box(
    buf: &[u8],
    i: usize,
    end: usize,
) -> Option<([u8; 4], std::ops::Range<usize>, usize)> {
    let size = u32::from_be_bytes(buf.get(i..i.checked_add(4)?)?.try_into().ok()?) as usize;
    let typ: [u8; 4] = buf.get(i + 4..i + 8)?.try_into().ok()?;
    let (hdr, sz) = match size {
        0 => (8, end.checked_sub(i)?), // box extends to the end
        1 => {
            let big = u64::from_be_bytes(buf.get(i + 8..i + 16)?.try_into().ok()?);
            (16, usize::try_from(big).ok()?)
        }
        s => (8, s),
    };
    let box_end = i.checked_add(sz)?;
    if sz < hdr || box_end > end {
        return None;
    }
    Some((typ, i + hdr..box_end, box_end))
}

/// First child box of `range` with the given fourcc:
/// `(payload_range, box_range)`.
pub(super) fn find_box(
    buf: &[u8],
    range: std::ops::Range<usize>,
    fourcc: &[u8; 4],
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut i = range.start;
    while i + 8 <= range.end {
        let (typ, payload, box_end) = next_box(buf, i, range.end)?;
        if typ == *fourcc {
            return Some((payload, i..box_end));
        }
        i = box_end;
    }
    None
}

/// The `meta` payload (past the FullBox version/flags) of an AVIF file.
pub(super) fn meta_payload(avif: &[u8]) -> Option<std::ops::Range<usize>> {
    let (meta, _) = find_box(avif, 0..avif.len(), b"meta")?;
    Some(meta.start.checked_add(4)?..meta.end)
}

/// The primary item id from `pitm`.
pub(super) fn primary_item_id(avif: &[u8], meta: std::ops::Range<usize>) -> Option<u32> {
    let (pitm, _) = find_box(avif, meta, b"pitm")?;
    let p = avif.get(pitm.clone())?;
    Some(if *p.first()? == 0 {
        u16::from_be_bytes(p.get(4..6)?.try_into().ok()?) as u32
    } else {
        u32::from_be_bytes(p.get(4..8)?.try_into().ok()?)
    })
}

/// Walk an `ipma` payload, calling `f(item_id, prop_index)` for every
/// association; returns the byte width of association entries and, for
/// `item`, the offsets of its association-count byte and entry end.
pub(super) struct IpmaEntry {
    count_pos: usize,
    entry_end: usize,
    wide: bool,
}

pub(super) fn ipma_walk(p: &[u8], item: u32, mut f: impl FnMut(u32, usize)) -> Option<IpmaEntry> {
    let version = *p.first()?;
    let wide = p.get(3)? & 1 == 1;
    let entry_count = u32::from_be_bytes(p.get(4..8)?.try_into().ok()?);
    let mut off = 8usize;
    let mut found = None;
    // Still images carry a handful of items; 64 bounds hostile counts.
    for _ in 0..entry_count.min(64) {
        let item_id = if version < 1 {
            let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
            off += 2;
            v
        } else {
            let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
            off += 4;
            v
        };
        let count_pos = off;
        let assoc_count = *p.get(off)? as usize;
        off += 1;
        for _ in 0..assoc_count {
            let idx = if wide {
                let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0x7FFF;
                off += 2;
                v as usize
            } else {
                let v = (*p.get(off)? & 0x7F) as usize;
                off += 1;
                v
            };
            f(item_id, idx);
        }
        if item_id == item {
            found = Some(IpmaEntry {
                count_pos,
                entry_end: off,
                wide,
            });
        }
    }
    found
}

/// Extract the primary item's ICC profile (`colr` box of colour type
/// `prof`/`ricc`) from an AVIF container. `None` for a missing profile
/// or anything malformed — never an error.
pub fn extract_icc(avif: &[u8]) -> Option<Vec<u8>> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iprp, _) = find_box(avif, meta, b"iprp")?;
    let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
    // ipco children in order; association indices are 1-based.
    let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
    let mut i = ipco.start;
    // 1024 comfortably covers wide (15-bit) association indices seen
    // in practice while bounding hostile property counts.
    while i + 8 <= ipco.end && props.len() < 1024 {
        let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
        props.push((typ, payload));
        i = box_end;
    }
    let (ipma, _) = find_box(avif, iprp, b"ipma")?;
    let mut icc = None;
    ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
        if item_id != primary || icc.is_some() || idx == 0 {
            return;
        }
        if let Some((typ, payload)) = props.get(idx - 1)
            && typ == b"colr"
        {
            let c = &avif[payload.clone()];
            if (c.get(0..4) == Some(b"prof".as_slice()) || c.get(0..4) == Some(b"ricc".as_slice()))
                && c.len() > 4
                && c.len() - 4 <= crate::pipeline::ICC_CAP
            {
                icc = Some(c[4..].to_vec());
            }
        }
    })?;
    icc
}

/// The primary item's `irot`/`imir` transforms composed as an
/// EXIF-style orientation. MIAF-conformant writers associate the
/// rotation before the mirror and HEIF applies transforms in
/// association order, so both orders are honored: mirror-first files
/// (non-MIAF but spec-legal, and rendered that way by libheif) use the
/// dihedral identity `rot_a ∘ mirror = mirror ∘ rot_{-a}` to reduce to
/// the rotation-first table. Upright on anything absent or malformed.
pub(crate) fn extract_orientation(avif: &[u8]) -> crate::meta::Orientation {
    fn inner(avif: &[u8]) -> Option<(u8, Option<u8>, bool)> {
        let meta = meta_payload(avif)?;
        let primary = primary_item_id(avif, meta.clone())?;
        let (iprp, _) = find_box(avif, meta, b"iprp")?;
        let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
        let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
        let mut i = ipco.start;
        while i + 8 <= ipco.end && props.len() < 1024 {
            let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
            props.push((typ, payload));
            i = box_end;
        }
        let (ipma, _) = find_box(avif, iprp, b"ipma")?;
        let mut angle = 0u8;
        let mut mirror: Option<u8> = None;
        let mut saw_irot = false;
        let mut mirror_first = false;
        ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
            if item_id != primary || idx == 0 {
                return;
            }
            let Some((typ, payload)) = props.get(idx - 1) else {
                return;
            };
            match typ {
                b"irot" if !saw_irot => {
                    if let Some(&a) = avif[payload.clone()].first() {
                        angle = a & 3;
                        saw_irot = true;
                    }
                }
                b"imir" if mirror.is_none() => {
                    mirror = avif[payload.clone()].first().map(|m| m & 1);
                    mirror_first = mirror.is_some() && !saw_irot;
                }
                _ => {}
            }
        })?;
        Some((angle, mirror, mirror_first && saw_irot))
    }
    match inner(avif) {
        Some((angle, mirror, mirror_first)) => {
            let angle = if mirror_first { (4 - angle) & 3 } else { angle };
            crate::meta::Orientation::from_rot_mirror(angle, mirror)
        }
        None => crate::meta::Orientation::UPRIGHT,
    }
}

/// Splice `icc` into an AVIF container as a `colr` (`prof`) property
/// associated with the primary item: the property is appended to
/// `ipco` (existing 1-based indices keep their meaning), one
/// association is appended to the primary item's `ipma` entry, the
/// enclosing box sizes grow accordingly, and absolute `iloc` offsets
/// shift by the inserted length. The property surgery is proven by
/// re-extraction before the result ships; the `iloc` patch is *not*
/// covered by that proof (extraction reads properties, not item
/// data), so anything the patcher does not fully recognize — exotic
/// versions, oversized item tables — is refused outright rather than
/// partially patched. `None` always means "leave the container
/// unprofiled". In production the input is always our own
/// serializer's output (see `finish_avif`); the layout-agnostic
/// parsing is defense against that dependency evolving, and the
/// decode-roundtrip tests pin the layout actually in use.
pub(crate) fn embed_icc(avif: &[u8], icc: &[u8]) -> Option<Vec<u8>> {
    let meta_pl = meta_payload(avif)?;
    let (_, meta_box) = find_box(avif, 0..avif.len(), b"meta")?;
    let primary = primary_item_id(avif, meta_pl.clone())?;
    let (iprp_pl, iprp_box) = find_box(avif, meta_pl.clone(), b"iprp")?;
    let (ipco_pl, ipco_box) = find_box(avif, iprp_pl.clone(), b"ipco")?;
    let mut prop_count = 0usize;
    let mut i = ipco_pl.start;
    while i + 8 <= ipco_pl.end {
        let (_, _, box_end) = next_box(avif, i, ipco_pl.end)?;
        prop_count += 1;
        i = box_end;
    }
    let (ipma_pl, ipma_box) = find_box(avif, iprp_pl, b"ipma")?;
    let entry = ipma_walk(avif.get(ipma_pl.clone())?, primary, |_, _| {})?;
    let new_idx = prop_count + 1;
    if new_idx > if entry.wide { 0x7FFF } else { 0x7F } {
        return None;
    }

    // colr box: size + "colr" + "prof" + profile bytes.
    let colr_len = 12 + icc.len();
    let mut colr = Vec::with_capacity(colr_len);
    colr.extend(u32::try_from(colr_len).ok()?.to_be_bytes());
    colr.extend_from_slice(b"colr");
    colr.extend_from_slice(b"prof");
    colr.extend_from_slice(icc);
    // avif-serialize writes narrow associations today, so the wide arm
    // is reachable only if that changes; the extractor's wide arm, by
    // contrast, runs on arbitrary third-party sources.
    let assoc: Vec<u8> = if entry.wide {
        (new_idx as u16).to_be_bytes().to_vec()
    } else {
        vec![new_idx as u8]
    };

    // Two insertion points, in file order (ipco precedes ipma inside
    // iprp in every writer we consume, but nothing below assumes it).
    let ins_colr = ipco_pl.end;
    let ins_assoc = ipma_pl.start + entry.entry_end;
    let (first, second) = if ins_colr <= ins_assoc {
        ((ins_colr, &colr), (ins_assoc, &assoc))
    } else {
        ((ins_assoc, &assoc), (ins_colr, &colr))
    };
    let delta = colr.len() + assoc.len();
    let mut out = Vec::with_capacity(avif.len() + delta);
    out.extend_from_slice(&avif[..first.0]);
    out.extend_from_slice(first.1);
    out.extend_from_slice(&avif[first.0..second.0]);
    out.extend_from_slice(second.1);
    out.extend_from_slice(&avif[second.0..]);

    // A position in the original maps into `out` shifted by whatever
    // was inserted before it.
    let shift = |pos: usize| -> usize {
        pos + if pos >= second.0 {
            delta
        } else if pos >= first.0 {
            first.1.len()
        } else {
            0
        }
    };
    // Grow the enclosing box sizes (size field = first 4 bytes of the
    // box; all four are ordinary compact-size boxes here).
    for (bx, grow) in [
        (&meta_box, delta),
        (&iprp_box, delta),
        (&ipco_box, colr.len()),
        (&ipma_box, assoc.len()),
    ] {
        let at = shift(bx.start);
        let old = u32::from_be_bytes(out.get(at..at + 4)?.try_into().ok()?);
        let new = old.checked_add(u32::try_from(grow).ok()?)?;
        out.get_mut(at..at + 4)?.copy_from_slice(&new.to_be_bytes());
    }
    // One more association on the primary item's entry.
    let count_at = shift(ipma_pl.start + entry.count_pos);
    let c = *out.get(count_at)?;
    if c == u8::MAX {
        return None;
    }
    *out.get_mut(count_at)? = c + 1;
    // Absolute iloc offsets move by however much landed before them.
    patch_iloc(avif, &mut out, meta_pl, &shift, |pos| {
        if pos >= second.0 {
            delta as u64
        } else if pos >= first.0 {
            first.1.len() as u64
        } else {
            0
        }
    })?;

    // Prove the surgery before shipping it.
    (extract_icc(&out).as_deref() == Some(icc)).then_some(out)
}

/// Add `value_delta(original_target)` to every absolute file offset in
/// the `iloc` box (construction method 0). Offset *fields* are located
/// on the original buffer and patched through `shift`.
pub(super) fn patch_iloc(
    avif: &[u8],
    out: &mut [u8],
    meta: std::ops::Range<usize>,
    shift: &dyn Fn(usize) -> usize,
    value_delta: impl Fn(usize) -> u64,
) -> Option<()> {
    let (iloc, _) = find_box(avif, meta, b"iloc")?;
    let p = avif.get(iloc.clone())?;
    let version = *p.first()?;
    let mut off = 4usize;
    let sizes = *p.get(off)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xF) as usize);
    let sizes2 = *p.get(off + 1)?;
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    off += 2;
    if ![0, 4, 8].contains(&offset_size) || ![0, 4, 8].contains(&base_offset_size) {
        return None;
    }
    let item_count = if version < 2 {
        let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
        off += 2;
        v
    } else {
        let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
        off += 4;
        v
    };
    // Patch an offset field of `size` bytes at iloc-payload offset `at`.
    let patch = |out: &mut [u8], at: usize, size: usize| -> Option<()> {
        if size == 0 {
            return Some(());
        }
        let val = if size == 4 {
            u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?) as u64
        } else {
            u64::from_be_bytes(p.get(at..at + 8)?.try_into().ok()?)
        };
        let target = usize::try_from(val).ok()?;
        let new = val.checked_add(value_delta(target))?;
        let dst = shift(iloc.start + at);
        if size == 4 {
            out.get_mut(dst..dst + 4)?
                .copy_from_slice(&u32::try_from(new).ok()?.to_be_bytes());
        } else {
            out.get_mut(dst..dst + 8)?
                .copy_from_slice(&new.to_be_bytes());
        }
        Some(())
    };
    // embed_icc only ever patches our own serializer's output (a
    // handful of items); anything bigger is refused outright, because
    // a *partially* patched iloc would not be caught by the caller's
    // re-extraction check (which reads properties, not item data).
    if item_count > 64 {
        return None;
    }
    for _ in 0..item_count {
        off += if version < 2 { 2 } else { 4 }; // item_id
        let method = if version >= 1 {
            let m = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0xF;
            off += 2;
            m
        } else {
            0
        };
        off += 2; // data_reference_index
        let base_at = off;
        off += base_offset_size;
        if method == 0 && base_offset_size > 0 {
            patch(out, base_at, base_offset_size)?;
        }
        let extent_count = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as usize;
        off += 2;
        if extent_count > 64 {
            return None;
        }
        for _ in 0..extent_count {
            off += index_size;
            let ext_at = off;
            off += offset_size;
            off += length_size;
            if method == 0 && base_offset_size == 0 {
                patch(out, ext_at, offset_size)?;
            }
        }
    }
    Some(())
}

/// Does the container declare an image sequence (`avis` brand)?
/// MIAF requires such files to also carry a valid still primary item,
/// which is what the first-frame fallbacks below read.
pub(super) fn is_animated_brand(data: &[u8]) -> bool {
    match find_box(data, 0..data.len(), b"ftyp") {
        Some((pl, _)) => data[pl].chunks(4).any(|b| b == b"avis"),
        None => false,
    }
}

/// The raw AV1 payload of the primary item, assembled from its `iloc`
/// extents (construction method 0 — absolute file offsets). Bounds-
/// checked throughout; `None` on anything the walk does not fully
/// recognize.
pub(super) fn primary_item_bytes(avif: &[u8]) -> Option<Vec<u8>> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iloc, _) = find_box(avif, meta, b"iloc")?;
    let p = avif.get(iloc)?;
    let version = *p.first()?;
    let sizes = *p.get(4)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xF) as usize);
    let sizes2 = *p.get(5)?;
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    if ![0, 4, 8].contains(&offset_size)
        || ![0, 4, 8].contains(&length_size)
        || ![0, 4, 8].contains(&base_offset_size)
    {
        return None;
    }
    let mut off = 6usize;
    let read_u = |off: &mut usize, size: usize| -> Option<u64> {
        let v = match size {
            0 => 0,
            4 => u32::from_be_bytes(p.get(*off..*off + 4)?.try_into().ok()?) as u64,
            8 => u64::from_be_bytes(p.get(*off..*off + 8)?.try_into().ok()?),
            _ => return None,
        };
        *off += size;
        Some(v)
    };
    let item_count = if version < 2 {
        let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
        off += 2;
        v
    } else {
        let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
        off += 4;
        v
    };
    for _ in 0..item_count.min(64) {
        let item_id = if version < 2 {
            let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
            off += 2;
            v
        } else {
            let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
            off += 4;
            v
        };
        let method = if version >= 1 {
            let m = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0xF;
            off += 2;
            m
        } else {
            0
        };
        off += 2; // data_reference_index
        let base = read_u(&mut off, base_offset_size)?;
        let extent_count = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as usize;
        off += 2;
        if extent_count > 64 {
            return None;
        }
        let mut out: Option<Vec<u8>> = (item_id == primary && method == 0).then(Vec::new);
        for _ in 0..extent_count {
            off += index_size;
            let ext_off = read_u(&mut off, offset_size)?;
            let ext_len = read_u(&mut off, length_size)?;
            if let Some(out) = out.as_mut() {
                let start = usize::try_from(base.checked_add(ext_off)?).ok()?;
                let len = usize::try_from(ext_len).ok()?;
                out.extend_from_slice(avif.get(start..start.checked_add(len)?)?);
            }
        }
        if let Some(out) = out {
            return (!out.is_empty()).then_some(out);
        }
    }
    None
}

/// The primary item's `ispe` dimensions via the property associations.
pub(super) fn primary_ispe(avif: &[u8]) -> Option<(usize, usize)> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iprp, _) = find_box(avif, meta, b"iprp")?;
    let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
    let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
    let mut i = ipco.start;
    while i + 8 <= ipco.end && props.len() < 1024 {
        let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
        props.push((typ, payload));
        i = box_end;
    }
    let (ipma, _) = find_box(avif, iprp, b"ipma")?;
    let mut dims = None;
    ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
        if item_id != primary || dims.is_some() || idx == 0 {
            return;
        }
        if let Some((typ, payload)) = props.get(idx - 1)
            && typ == b"ispe"
        {
            let p = &avif[payload.clone()];
            if let (Some(w), Some(h)) = (p.get(4..8), p.get(8..12)) {
                let w = u32::from_be_bytes(w.try_into().unwrap()) as usize;
                let h = u32::from_be_bytes(h.try_into().unwrap()) as usize;
                if w > 0 && h > 0 {
                    dims = Some((w, h));
                }
            }
        }
    })?;
    dims
}
