//! Imposition tools: booklet, n-up, step-and-repeat and split.
//!
//! Two stages:
//!   1. `make_plan`  - pure layout maths (no file access). Used for the live preview.
//!   2. `impose`     - loads the PDF, wraps every source page in a Form XObject and
//!                     draws those forms onto new sheets following the plan.
//! All sizes inside a plan are PDF points (1/72 inch); options come in as millimetres.

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;

const MM: f64 = 72.0 / 25.4;

// ------------------------------------------------------------------ options

#[derive(Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Opts {
    /// booklet | nup | repeat | split
    pub mode: String,
    /// auto | a6 | a5 | a4 | a3 | a2 | sra3 | b5 | b4 | letter | legal | tabloid | 12x18 | 13x19 | custom
    pub sheet: String,
    pub custom_w: f64,
    pub custom_h: f64,
    /// auto | portrait | landscape
    pub orient: String,
    pub margin: f64,
    pub gap_x: f64,
    pub gap_y: f64,
    pub cols: u32,
    pub rows: u32,
    /// across | down
    pub order: String,
    /// fit | actual
    pub scale: String,
    pub rotate_fit: bool,
    pub borders: bool,
    pub marks: bool,
    /// right-to-left binding (booklet) / right-to-left reading order (n-up, split)
    pub rtl: bool,
    /// booklet creep: millimetres each nested sheet moves toward the spine
    pub creep: f64,
    /// booklet: pages per section (0 = one booklet); rounded up to a multiple of 4
    pub section: u32,
    /// split: overlap between tiles in millimetres
    pub overlap: f64,
    /// page selection such as "1-4, 7, 9-"; empty = all pages
    pub pages: String,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            mode: "booklet".into(),
            sheet: "auto".into(),
            custom_w: 210.0,
            custom_h: 297.0,
            orient: "auto".into(),
            margin: 0.0,
            gap_x: 0.0,
            gap_y: 0.0,
            cols: 2,
            rows: 2,
            order: "across".into(),
            scale: "fit".into(),
            rotate_fit: false,
            borders: false,
            marks: false,
            rtl: false,
            creep: 0.0,
            section: 0,
            overlap: 0.0,
            pages: String::new(),
        }
    }
}

// --------------------------------------------------------------------- plan

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    /// 0-based index of the source page; None = blank slot
    pub src: Option<usize>,
    /// where the page lands on the sheet (points, origin bottom-left)
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
    pub rot: bool,
    /// split: the part of the source page to show [x, y, w, h] in source points
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crop: Option<[f64; 4]>,
    /// clip rectangle [x, y, w, h] on the sheet
    #[serde(skip)]
    pub clip: Option<[f64; 4]>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Sheet {
    pub w: f64,
    pub h: f64,
    pub label: String,
    pub cells: Vec<Cell>,
    #[serde(skip)]
    pub fold: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub sheets: Vec<Sheet>,
    /// number of sheets before the preview was truncated
    pub total: usize,
    pub source_pages: usize,
    pub selected: usize,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
struct R {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn preset(name: &str) -> Option<(f64, f64)> {
    Some(match name {
        "a6" => (105.0, 148.0),
        "a5" => (148.0, 210.0),
        "a4" => (210.0, 297.0),
        "a3" => (297.0, 420.0),
        "a2" => (420.0, 594.0),
        "sra3" => (320.0, 450.0),
        "b5" => (176.0, 250.0),
        "b4" => (250.0, 353.0),
        "letter" => (215.9, 279.4),
        "legal" => (215.9, 355.6),
        "tabloid" => (279.4, 431.8),
        "12x18" => (304.8, 457.2),
        "13x19" => (330.2, 482.6),
        _ => return None,
    })
}

/// "1-3, 5, 8-" -> 0-based indexes in the order written
pub fn parse_pages(spec: &str, n: usize) -> Result<Vec<usize>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok((0..n).collect());
    }
    let mut out = Vec::new();
    for tok in spec.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        let num = |s: &str| -> Result<usize, String> {
            s.trim()
                .parse::<usize>()
                .map_err(|_| format!("“{}” is not a valid page range", t))
        };
        let (a, b) = if let Some((l, r)) = t.split_once('-') {
            let a = if l.trim().is_empty() { 1 } else { num(l)? };
            let b = if r.trim().is_empty() { n } else { num(r)? };
            (a, b)
        } else {
            let a = num(t)?;
            (a, a)
        };
        if a == 0 || b == 0 || a > n || b > n {
            return Err(format!("Page {} is outside this PDF (it has {} pages)", a.max(b), n));
        }
        if a <= b {
            out.extend((a - 1)..b);
        } else {
            out.extend(((b - 1)..a).rev());
        }
    }
    if out.is_empty() {
        return Err("No pages selected".into());
    }
    Ok(out)
}

fn place(pw: f64, ph: f64, c: &R, o: &Opts, can_rot: bool) -> (R, f64, bool) {
    let fit = |w: f64, h: f64| (c.w / w).min(c.h / h);
    let mut rot = false;
    let s = if o.scale == "actual" {
        if can_rot && o.rotate_fit {
            let fits = |w: f64, h: f64| w <= c.w + 0.01 && h <= c.h + 0.01;
            if !fits(pw, ph) && fits(ph, pw) {
                rot = true;
            }
        }
        1.0
    } else {
        let s0 = fit(pw, ph);
        if can_rot && o.rotate_fit {
            let s1 = fit(ph, pw);
            if s1 > s0 * 1.005 {
                rot = true;
                s1
            } else {
                s0
            }
        } else {
            s0
        }
    };
    let (w, h) = if rot { (ph * s, pw * s) } else { (pw * s, ph * s) };
    (
        R {
            x: c.x + (c.w - w) / 2.0,
            y: c.y + (c.h - h) / 2.0,
            w,
            h,
        },
        s,
        rot,
    )
}

/// cells of a cols x rows grid in reading order
fn grid(w: f64, h: f64, o: &Opts, cols: usize, rows: usize) -> Result<Vec<R>, String> {
    let m = o.margin.max(0.0) * MM;
    let gx = o.gap_x.max(0.0) * MM;
    let gy = o.gap_y.max(0.0) * MM;
    let cw = (w - 2.0 * m - (cols as f64 - 1.0) * gx) / cols as f64;
    let ch = (h - 2.0 * m - (rows as f64 - 1.0) * gy) / rows as f64;
    if cw < 10.0 || ch < 10.0 {
        return Err("The margins and gaps leave no room for the pages on this sheet size".into());
    }
    let rect = |c: usize, r: usize| R {
        x: m + c as f64 * (cw + gx),
        y: h - m - (r as f64 + 1.0) * ch - r as f64 * gy,
        w: cw,
        h: ch,
    };
    let mut v = Vec::with_capacity(cols * rows);
    let down = o.order == "down";
    let rtl = o.rtl && o.mode != "booklet";
    let col_of = |c: usize| if rtl { cols - 1 - c } else { c };
    if down {
        for c in 0..cols {
            for r in 0..rows {
                v.push(rect(col_of(c), r));
            }
        }
    } else {
        for r in 0..rows {
            for c in 0..cols {
                v.push(rect(col_of(c), r));
            }
        }
    }
    Ok(v)
}

fn sheet_dims(o: &Opts, first: (f64, f64), cols: usize, rows: usize) -> Result<(f64, f64), String> {
    let m = o.margin.max(0.0) * MM;
    if o.sheet == "auto" && o.mode == "booklet" {
        return Ok((2.0 * first.0 + o.gap_x.max(0.0) * MM + 2.0 * m, first.1 + 2.0 * m));
    }
    let (pw, ph) = if o.sheet == "custom" {
        (o.custom_w, o.custom_h)
    } else {
        preset(&o.sheet).or_else(|| preset("a4")).unwrap()
    };
    if pw < 20.0 || ph < 20.0 {
        return Err("The custom sheet size is too small".into());
    }
    let (sw, sl) = (pw.min(ph) * MM, pw.max(ph) * MM); // short, long side in pt
    let orient = if o.orient == "auto" && o.mode == "booklet" { "landscape" } else { o.orient.as_str() };
    match orient {
        "portrait" => Ok((sw, sl)),
        "landscape" => Ok((sl, sw)),
        _ => {
            // pick the orientation that makes the first page larger
            let score = |w: f64, h: f64| -> f64 {
                match grid(w, h, o, cols, rows) {
                    Ok(g) => {
                        let (_, s, _) = place(first.0, first.1, &g[0], o, true);
                        s
                    }
                    Err(_) => 0.0,
                }
            };
            if score(sl, sw) > score(sw, sl) * 1.001 {
                Ok((sl, sw))
            } else {
                Ok((sw, sl))
            }
        }
    }
}

fn blank_cell(c: &R) -> Cell {
    Cell { src: None, x: c.x, y: c.y, w: c.w, h: c.h, scale: 1.0, rot: false, crop: None, clip: None }
}

/// `sizes` = effective (rotation applied) width/height in points of every source page
pub fn make_plan(sizes: &[(f64, f64)], opts: &Opts) -> Result<Plan, String> {
    if sizes.is_empty() {
        return Err("This PDF has no pages".into());
    }
    let mut o = opts.clone();
    let mut notes = Vec::new();
    if o.marks && o.mode != "split" && o.margin < 6.0 {
        o.margin = 6.0;
        notes.push("Margin raised to 6 mm so the marks have room.".to_string());
    }
    let sel = parse_pages(&o.pages, sizes.len())?;
    let first = sizes[sel[0]];
    let mut sheets: Vec<Sheet> = Vec::new();

    match o.mode.as_str() {
        "booklet" => {
            let (w, h) = sheet_dims(&o, first, 2, 1)?;
            let cells = grid(w, h, &o, 2, 1)?;
            let sec = if o.section == 0 { usize::MAX } else { ((o.section as usize + 3) / 4 * 4).max(4) };
            let creep = o.creep.max(0.0) * MM;
            let mut total_blank = 0;
            for (si, chunk) in sel.chunks(sec).enumerate() {
                let mut p: Vec<Option<usize>> = chunk.iter().map(|&i| Some(i)).collect();
                while p.len() % 4 != 0 {
                    p.push(None);
                    total_blank += 1;
                }
                let n = p.len();
                let nsec = if sec == usize::MAX { 1 } else { (sel.len() + sec - 1) / sec };
                for i in 0..n / 4 {
                    let sides = [
                        (p[n - 1 - 2 * i], p[2 * i], "front"),
                        (p[2 * i + 1], p[n - 2 - 2 * i], "back"),
                    ];
                    for (l, r, side) in sides {
                        let (l, r) = if o.rtl { (r, l) } else { (l, r) };
                        let shift = i as f64 * creep;
                        let mut cs = Vec::new();
                        for (k, pg) in [l, r].iter().enumerate() {
                            match pg {
                                Some(idx) => {
                                    let (pw, ph) = sizes[*idx];
                                    let (mut rr, s, _) = place(pw, ph, &cells[k], &o, false);
                                    rr.x += if k == 0 { shift } else { -shift };
                                    cs.push(Cell {
                                        src: Some(*idx),
                                        x: rr.x,
                                        y: rr.y,
                                        w: rr.w,
                                        h: rr.h,
                                        scale: s,
                                        rot: false,
                                        crop: None,
                                        clip: if o.scale == "actual" { Some([cells[k].x, cells[k].y, cells[k].w, cells[k].h]) } else { None },
                                    });
                                }
                                None => cs.push(blank_cell(&cells[k])),
                            }
                        }
                        let label = if nsec > 1 {
                            format!("Section {} · sheet {} · {}", si + 1, i + 1, side)
                        } else {
                            format!("Sheet {} · {}", i + 1, side)
                        };
                        sheets.push(Sheet { w, h, label, cells: cs, fold: true });
                    }
                }
            }
            if total_blank > 0 {
                notes.push(format!(
                    "{} blank page{} added so the booklet fills whole sheets.",
                    total_blank,
                    if total_blank == 1 { "" } else { "s" }
                ));
            }
            if o.creep > 0.0 {
                notes.push("Creep is applied per nested sheet; the innermost sheet moves furthest toward the spine.".into());
            }
            notes.push("Print double-sided and flip on the short edge. Fold and nest the sheets in order.".into());
        }
        "nup" | "repeat" => {
            let cols = o.cols.clamp(1, 20) as usize;
            let rows = o.rows.clamp(1, 20) as usize;
            let (w, h) = sheet_dims(&o, first, cols, rows)?;
            let cells = grid(w, h, &o, cols, rows)?;
            let per = cols * rows;
            let mk = |idx: usize, c: &R| -> Cell {
                let (pw, ph) = sizes[idx];
                let (rr, s, rot) = place(pw, ph, c, &o, true);
                Cell {
                    src: Some(idx),
                    x: rr.x,
                    y: rr.y,
                    w: rr.w,
                    h: rr.h,
                    scale: s,
                    rot,
                    crop: None,
                    clip: if o.scale == "actual" { Some([c.x, c.y, c.w, c.h]) } else { None },
                }
            };
            if o.mode == "nup" {
                for (si, chunk) in sel.chunks(per).enumerate() {
                    let cs: Vec<Cell> = (0..per)
                        .map(|k| match chunk.get(k) {
                            Some(&idx) => mk(idx, &cells[k]),
                            None => blank_cell(&cells[k]),
                        })
                        .collect();
                    sheets.push(Sheet { w, h, label: format!("Sheet {}", si + 1), cells: cs, fold: false });
                }
            } else {
                for (si, &idx) in sel.iter().enumerate() {
                    let cs: Vec<Cell> = cells.iter().map(|c| mk(idx, c)).collect();
                    sheets.push(Sheet {
                        w,
                        h,
                        label: format!("Sheet {} · page {} × {}", si + 1, idx + 1, per),
                        cells: cs,
                        fold: false,
                    });
                }
            }
        }
        "split" => {
            let cols = o.cols.clamp(1, 20) as usize;
            let rows = o.rows.clamp(1, 20) as usize;
            if cols * rows < 2 {
                return Err("Choose at least 2 pieces to split each page into".into());
            }
            let ov = o.overlap.max(0.0) * MM;
            for &idx in &sel {
                let (pw, ph) = sizes[idx];
                let tw = (pw + (cols as f64 - 1.0) * ov) / cols as f64;
                let th = (ph + (rows as f64 - 1.0) * ov) / rows as f64;
                // output page for one tile
                let (sw, sh) = if o.sheet == "auto" {
                    (tw, th)
                } else {
                    sheet_dims(&o, (tw, th), 1, 1)?
                };
                let area = if o.sheet == "auto" {
                    R { x: 0.0, y: 0.0, w: tw, h: th }
                } else {
                    let g = grid(sw, sh, &Opts { mode: "split".into(), cols: 1, rows: 1, ..o.clone() }, 1, 1)?;
                    g[0]
                };
                let mut tiles: Vec<(usize, usize)> = Vec::new(); // (col, row)
                if o.order == "down" {
                    for c in 0..cols {
                        for r in 0..rows {
                            tiles.push((c, r));
                        }
                    }
                } else {
                    for r in 0..rows {
                        for c in 0..cols {
                            tiles.push((c, r));
                        }
                    }
                }
                if o.rtl {
                    tiles = tiles.into_iter().map(|(c, r)| (cols - 1 - c, r)).collect();
                }
                for (n, (c, r)) in tiles.into_iter().enumerate() {
                    let cx = c as f64 * (tw - ov);
                    let cy = ph - (r as f64 * (th - ov) + th); // bottom edge in PDF space
                    let (rr, s, _) = place(tw, th, &area, &o, false);
                    let (rr, s) = if o.sheet == "auto" { (R { x: 0.0, y: 0.0, w: tw, h: th }, 1.0) } else { (rr, s) };
                    sheets.push(Sheet {
                        w: sw,
                        h: sh,
                        label: format!("Page {} · piece {}", idx + 1, n + 1),
                        cells: vec![Cell {
                            src: Some(idx),
                            x: rr.x,
                            y: rr.y,
                            w: rr.w,
                            h: rr.h,
                            scale: s,
                            rot: false,
                            crop: Some([cx, cy, tw, th]),
                            clip: Some([rr.x, rr.y, rr.w, rr.h]),
                        }],
                        fold: false,
                    });
                }
            }
        }
        _ => return Err("Unknown imposition mode".into()),
    }

    let total = sheets.len();
    Ok(Plan { sheets, total, source_pages: sizes.len(), selected: sel.len(), notes })
}

// -------------------------------------------------------------- PDF reading

#[derive(Clone, Copy, Debug)]
struct Geo {
    x0: f64,
    y0: f64,
    w: f64,
    h: f64,
    rot: i64,
    ew: f64,
    eh: f64,
}

fn inherited(doc: &Document, mut id: ObjectId, key: &[u8]) -> Option<Object> {
    for _ in 0..64 {
        let d = doc.get_dictionary(id).ok()?;
        if let Ok(o) = d.get(key) {
            return Some(o.clone());
        }
        id = d.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn num(doc: &Document, o: &Object) -> Option<f64> {
    match doc.dereference(o).ok()?.1 {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn rect_of(doc: &Document, o: &Object) -> Option<[f64; 4]> {
    let arr = doc.dereference(o).ok()?.1.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let v: Vec<f64> = arr.iter().take(4).filter_map(|x| num(doc, x)).collect();
    if v.len() != 4 {
        return None;
    }
    Some([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

fn geo(doc: &Document, id: ObjectId) -> Geo {
    let b = inherited(doc, id, b"CropBox")
        .and_then(|o| rect_of(doc, &o))
        .or_else(|| inherited(doc, id, b"MediaBox").and_then(|o| rect_of(doc, &o)))
        .unwrap_or([0.0, 0.0, 595.276, 841.89]);
    let rot = inherited(doc, id, b"Rotate")
        .and_then(|o| num(doc, &o))
        .map(|v| (((v as i64) % 360) + 360) % 360)
        .unwrap_or(0);
    let rot = (rot / 90) * 90;
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    let (ew, eh) = if rot == 90 || rot == 270 { (h, w) } else { (w, h) };
    Geo { x0: b[0], y0: b[1], w, h, rot, ew, eh }
}

fn load(path: &str) -> Result<Document, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open the PDF: {}", e))?;
    if doc.is_encrypted() {
        return Err("This PDF is password protected. Remove the password first.".into());
    }
    Ok(doc)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SizesOut {
    pub pages: usize,
    pub sizes: Vec<[f64; 2]>,
}

pub fn sizes(path: &str) -> Result<SizesOut, String> {
    let doc = load(path)?;
    let v: Vec<[f64; 2]> = doc
        .get_pages()
        .values()
        .map(|&id| {
            let g = geo(&doc, id);
            [g.ew, g.eh]
        })
        .collect();
    if v.is_empty() {
        return Err("This PDF has no pages".into());
    }
    Ok(SizesOut { pages: v.len(), sizes: v })
}

/// Preview plan containing every generated sheet.
pub fn plan(sizes: Vec<[f64; 2]>, opts: &Opts) -> Result<Plan, String> {
    let s: Vec<(f64, f64)> = sizes.iter().map(|p| (p[0], p[1])).collect();
    make_plan(&s, opts)
}

// ------------------------------------------------------------- PDF writing

fn r(v: f64) -> Object {
    Object::Real(v as f32)
}

fn fnum(v: f64) -> String {
    let s = format!("{:.3}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

fn make_form(doc: &mut Document, pid: ObjectId, g: &Geo) -> Result<ObjectId, String> {
    let content = doc.get_page_content(pid).map_err(|e| format!("Could not read a page: {}", e))?;
    let res = inherited(doc, pid, b"Resources").unwrap_or_else(|| Object::Dictionary(Dictionary::new()));
    // matrix that turns the (possibly rotated) page box into a box at 0,0 of size ew x eh
    let m = match g.rot {
        90 => [0.0, -1.0, 1.0, 0.0, -g.y0, g.w + g.x0],
        180 => [-1.0, 0.0, 0.0, -1.0, g.w + g.x0, g.h + g.y0],
        270 => [0.0, 1.0, -1.0, 0.0, g.h + g.y0, -g.x0],
        _ => [1.0, 0.0, 0.0, 1.0, -g.x0, -g.y0],
    };
    let dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "FormType" => 1,
        "BBox" => vec![r(g.x0), r(g.y0), r(g.x0 + g.w), r(g.y0 + g.h)],
        "Matrix" => m.iter().map(|v| r(*v)).collect::<Vec<Object>>(),
        "Resources" => res,
    };
    Ok(doc.add_object(Stream::new(dict, content)))
}

fn push_marks(c: &mut String, sh: &Sheet) {
    let len = 3.0 * MM;
    let off = 1.0 * MM;
    let live: Vec<&Cell> = sh.cells.iter().filter(|c| c.src.is_some()).collect();
    if live.is_empty() {
        return;
    }
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for k in &live {
        xs.push(k.x);
        xs.push(k.x + k.w);
        ys.push(k.y);
        ys.push(k.y + k.h);
    }
    let dedupe = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    };
    dedupe(&mut xs);
    dedupe(&mut ys);
    let top = ys.last().copied().unwrap();
    let bottom = ys[0];
    let left = xs[0];
    let right = xs.last().copied().unwrap();
    if sh.fold {
        xs.push(sh.w / 2.0);
    }
    c.push_str("q 0 G 0.3 w\n");
    for x in &xs {
        let _ = writeln!(c, "{} {} m {} {} l S", fnum(*x), fnum(top + off), fnum(*x), fnum(top + off + len));
        let _ = writeln!(c, "{} {} m {} {} l S", fnum(*x), fnum(bottom - off), fnum(*x), fnum(bottom - off - len));
    }
    for y in &ys {
        let _ = writeln!(c, "{} {} m {} {} l S", fnum(left - off), fnum(*y), fnum(left - off - len), fnum(*y));
        let _ = writeln!(c, "{} {} m {} {} l S", fnum(right + off), fnum(*y), fnum(right + off + len), fnum(*y));
    }
    c.push_str("Q\n");
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImposeOut {
    pub sheets: usize,
    pub bytes: u64,
    pub source_pages: usize,
    pub selected: usize,
}

pub fn impose(path: &str, opts: &Opts, out: &str, progress: &dyn Fn(usize, usize)) -> Result<ImposeOut, String> {
    let mut doc = load(path)?;
    let ids: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    if ids.is_empty() {
        return Err("This PDF has no pages".into());
    }
    let geos: Vec<Geo> = ids.iter().map(|&id| geo(&doc, id)).collect();
    let sizes: Vec<(f64, f64)> = geos.iter().map(|g| (g.ew, g.eh)).collect();
    let plan = make_plan(&sizes, opts)?;
    let o = {
        let mut o = opts.clone();
        if o.marks && o.mode != "split" && o.margin < 6.0 {
            o.margin = 6.0;
        }
        o
    };

    let pages_id = doc
        .catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(|p| p.as_reference())
        .map_err(|_| "This PDF has no page tree".to_string())?;

    let mut forms: HashMap<usize, ObjectId> = HashMap::new();
    let mut kids: Vec<Object> = Vec::new();
    let total = plan.sheets.len();

    for (si, sh) in plan.sheets.iter().enumerate() {
        let mut c = String::new();
        let mut xo = Dictionary::new();
        for cell in &sh.cells {
            let Some(idx) = cell.src else { continue };
            let fid = match forms.get(&idx) {
                Some(&f) => f,
                None => {
                    let f = make_form(&mut doc, ids[idx], &geos[idx])?;
                    forms.insert(idx, f);
                    f
                }
            };
            let name = format!("F{}", idx);
            xo.set(name.as_bytes().to_vec(), Object::Reference(fid));
            let s = cell.scale;
            let (cx, cy) = cell.crop.map(|b| (b[0], b[1])).unwrap_or((0.0, 0.0));
            c.push_str("q\n");
            if let Some(cl) = cell.clip {
                let _ = writeln!(c, "{} {} {} {} re W n", fnum(cl[0]), fnum(cl[1]), fnum(cl[2]), fnum(cl[3]));
            }
            if cell.rot {
                let _ = writeln!(
                    c,
                    "0 {} {} 0 {} {} cm",
                    fnum(s),
                    fnum(-s),
                    fnum(cell.x + cell.w),
                    fnum(cell.y)
                );
            } else {
                let _ = writeln!(
                    c,
                    "{} 0 0 {} {} {} cm",
                    fnum(s),
                    fnum(s),
                    fnum(cell.x - cx * s),
                    fnum(cell.y - cy * s)
                );
            }
            let _ = writeln!(c, "/{} Do\nQ", name);
            if o.borders && o.mode != "split" {
                let _ = writeln!(
                    c,
                    "q 0.5 G 0.4 w {} {} {} {} re S Q",
                    fnum(cell.x),
                    fnum(cell.y),
                    fnum(cell.w),
                    fnum(cell.h)
                );
            }
        }
        if o.marks && o.mode != "split" {
            push_marks(&mut c, sh);
        }
        let cid = doc.add_object(Stream::new(Dictionary::new(), c.into_bytes()));
        let page = dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![r(0.0), r(0.0), r(sh.w), r(sh.h)],
            "Resources" => dictionary! { "XObject" => Object::Dictionary(xo) },
            "Contents" => Object::Reference(cid),
        };
        kids.push(Object::Reference(doc.add_object(page)));
        progress(si + 1, total);
    }

    {
        let pd = doc.get_dictionary_mut(pages_id).map_err(|e| e.to_string())?;
        pd.set("Kids", Object::Array(kids));
        pd.set("Count", Object::Integer(total as i64));
        for k in [&b"Resources"[..], b"MediaBox", b"CropBox", b"Rotate"] {
            pd.remove(k);
        }
    }
    {
        // bookmarks, named destinations and form fields point at the old pages
        let cat = doc.catalog_mut().map_err(|e| e.to_string())?;
        for k in [
            &b"Outlines"[..],
            b"Names",
            b"Dests",
            b"OpenAction",
            b"AcroForm",
            b"StructTreeRoot",
            b"PageLabels",
            b"Threads",
            b"MarkInfo",
        ] {
            cat.remove(k);
        }
    }
    if doc.version.as_str() < "1.5" {
        doc.version = "1.5".to_string();
    }
    doc.prune_objects();
    doc.compress();
    doc.save(out).map_err(|e| format!("Could not save the PDF: {}", e))?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(ImposeOut { sheets: total, bytes, source_pages: plan.source_pages, selected: plan.selected })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a4(n: usize) -> Vec<(f64, f64)> {
        vec![(595.28, 841.89); n]
    }
    fn order(plan: &Plan) -> Vec<Vec<Option<usize>>> {
        plan.sheets.iter().map(|s| s.cells.iter().map(|c| c.src.map(|v| v + 1)).collect()).collect()
    }

    #[test]
    fn booklet_8() {
        let p = make_plan(&a4(8), &Opts::default()).unwrap();
        assert_eq!(
            order(&p),
            vec![
                vec![Some(8), Some(1)],
                vec![Some(2), Some(7)],
                vec![Some(6), Some(3)],
                vec![Some(4), Some(5)]
            ]
        );
    }

    #[test]
    fn booklet_pads_and_rtl() {
        let o = Opts { rtl: true, ..Opts::default() };
        let p = make_plan(&a4(6), &o).unwrap();
        assert_eq!(p.sheets.len(), 4);
        assert_eq!(order(&p)[0], vec![Some(1), None]);
    }

    #[test]
    fn pages_spec() {
        assert_eq!(parse_pages("1-3, 5, 8-", 9).unwrap(), vec![0, 1, 2, 4, 7, 8]);
        assert!(parse_pages("12", 9).is_err());
        assert_eq!(parse_pages("3-1", 9).unwrap(), vec![2, 1, 0]);
    }

    #[test]
    fn nup_counts() {
        let o = Opts { mode: "nup".into(), sheet: "a4".into(), cols: 2, rows: 2, ..Opts::default() };
        assert_eq!(make_plan(&a4(9), &o).unwrap().sheets.len(), 3);
    }

    #[test]
    fn split_tiles() {
        let o = Opts { mode: "split".into(), cols: 2, rows: 2, ..Opts::default() };
        assert_eq!(make_plan(&a4(2), &o).unwrap().sheets.len(), 8);
    }
}