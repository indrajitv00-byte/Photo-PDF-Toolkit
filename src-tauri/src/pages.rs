//! Page organizer and blank-page remover.
//!
//! Both tools do the same thing in the end: build a new PDF from an ordered list of
//! pages. Each entry says which original page to use, how far to turn it, or that it
//! is a new blank page. Deleting a page = leaving it out, duplicating = listing it
//! twice, reordering = changing the order of the list.

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PageSpec {
    /// 0-based index of the original page. For a new blank page this is the page
    /// whose size the blank page should copy.
    pub src: usize,
    /// extra clockwise turn in degrees (0, 90, 180, 270; negatives allowed)
    #[serde(default)]
    pub rot: i32,
    /// true = insert a new empty page instead of copying page `src`
    #[serde(default)]
    pub blank: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrangeOut {
    pub pages: usize,
    pub bytes: u64,
}

const INHERIT: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

fn load(path: &str) -> Result<Document, String> {
    let doc = Document::load(path).map_err(|e| format!("Could not open the PDF: {}", e))?;
    if doc.is_encrypted() {
        return Err("This PDF is password protected. Remove the password first.".into());
    }
    Ok(doc)
}

/// value of an inheritable page attribute, looking up through the /Parent chain
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

fn as_int(doc: &Document, o: &Object) -> Option<i64> {
    match doc.dereference(o).ok()?.1 {
        Object::Integer(i) => Some(*i),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

/// true for annotations that would point at pages/fields that no longer exist
fn is_dead_annot(doc: &Document, o: &Object) -> bool {
    let Ok((_, a)) = doc.dereference(o) else { return false };
    let Ok(d) = a.as_dict() else { return false };
    match d.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) {
        Some(b"Widget") => true,
        Some(b"Link") => {
            if d.has(b"Dest") {
                return true;
            }
            if let Ok(act) = d.get(b"A") {
                if let Ok((_, act)) = doc.dereference(act) {
                    if let Ok(ad) = act.as_dict() {
                        return matches!(ad.get(b"S").ok().and_then(|s| s.as_name().ok()), Some(b"GoTo"));
                    }
                }
            }
            false
        }
        _ => false,
    }
}

pub fn arrange(path: &str, specs: &[PageSpec], out: &str) -> Result<ArrangeOut, String> {
    if specs.is_empty() {
        return Err("There are no pages left to save.".into());
    }
    let mut doc = load(path)?;
    let pages: Vec<ObjectId> = doc.get_pages().values().copied().collect();
    if pages.is_empty() {
        return Err("This PDF has no pages".into());
    }
    if let Some(s) = specs.iter().find(|s| s.src >= pages.len()) {
        return Err(format!("Page {} does not exist in this PDF", s.src + 1));
    }

    let catalog_id = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .ok_or("The PDF has no catalog")?;
    let root_pages = doc
        .get_dictionary(catalog_id)
        .ok()
        .and_then(|c| c.get(b"Pages").ok())
        .and_then(|o| o.as_reference().ok())
        .ok_or("The PDF has no page tree")?;

    // 1. copy inherited attributes onto every original page so it survives being moved
    //    to a flat page tree, and drop links/fields that would point nowhere
    for &pid in &pages {
        let mut add: Vec<(&[u8], Object)> = Vec::new();
        for k in INHERIT {
            let has = doc.get_dictionary(pid).map(|d| d.has(k)).unwrap_or(false);
            if !has {
                if let Some(v) = inherited(&doc, pid, k) {
                    add.push((k, v));
                }
            }
        }
        let annots: Option<Vec<Object>> = doc
            .get_dictionary(pid)
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|a| doc.dereference(a).ok().map(|x| x.1.clone()))
            .and_then(|a| a.as_array().ok().cloned());
        let kept = annots.map(|v| v.into_iter().filter(|a| !is_dead_annot(&doc, a)).collect::<Vec<_>>());
        if let Ok(d) = doc.get_dictionary_mut(pid) {
            for (k, v) in add {
                d.set(k.to_vec(), v);
            }
            if let Some(k) = kept {
                if k.is_empty() {
                    d.remove(b"Annots");
                } else {
                    d.set("Annots", Object::Array(k));
                }
            }
            d.remove(b"StructParents");
        }
    }

    // 2. build the new page list (always start from the untouched original dictionaries,
    //    so a duplicate does not inherit the turn given to its twin)
    let originals: Vec<Dictionary> = pages
        .iter()
        .map(|&id| doc.get_dictionary(id).cloned().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let mut used = vec![false; pages.len()];
    let mut kids: Vec<Object> = Vec::with_capacity(specs.len());
    for s in specs {
        let pid = pages[s.src];
        let base = originals[s.src].clone();
        let base_rot = base.get(b"Rotate").ok().and_then(|o| as_int(&doc, o)).unwrap_or(0);
        let rot = (((base_rot + s.rot as i64) % 360) + 360) % 360;

        let mut d: Dictionary = if s.blank {
            let mut n = dictionary! {
                "Type" => "Page",
                "Resources" => Object::Dictionary(Dictionary::new()),
            };
            if let Ok(mb) = base.get(b"MediaBox") {
                n.set("MediaBox", mb.clone());
            } else {
                n.set("MediaBox", vec![0.into(), 0.into(), Object::Real(595.276), Object::Real(841.89)]);
            }
            if let Ok(cb) = base.get(b"CropBox") {
                n.set("CropBox", cb.clone());
            }
            n
        } else {
            base
        };
        d.set("Parent", Object::Reference(root_pages));
        if rot == 0 {
            d.remove(b"Rotate");
        } else {
            d.set("Rotate", Object::Integer(rot));
        }

        // the first use of an original page reuses its own object, a repeat gets a copy
        let id = if s.blank || used[s.src] {
            doc.add_object(Object::Dictionary(d))
        } else {
            used[s.src] = true;
            doc.objects.insert(pid, Object::Dictionary(d));
            pid
        };
        kids.push(Object::Reference(id));
    }

    // 3. flat page tree + remove things that point at pages by number or reference
    let count = kids.len() as i64;
    {
        let rp = doc.get_dictionary_mut(root_pages).map_err(|e| e.to_string())?;
        rp.set("Kids", Object::Array(kids));
        rp.set("Count", Object::Integer(count));
        for k in INHERIT {
            rp.remove(k);
        }
    }
    if let Ok(c) = doc.get_dictionary_mut(catalog_id) {
        for k in [
            "Outlines", "Names", "Dests", "AcroForm", "OpenAction", "PageLabels", "StructTreeRoot",
            "MarkInfo", "Threads", "AA",
        ] {
            c.remove(k.as_bytes());
        }
    }

    doc.prune_objects();
    doc.compress();
    doc.save(out).map_err(|e| format!("Could not save the PDF: {}", e))?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(ArrangeOut { pages: specs.len(), bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::{Content, Operation};
    use lopdf::Stream;

    /// builds an n-page PDF where page i has a Tj showing "P<i>", pages share one font
    fn sample(n: usize, path: &str) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let res = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
        let mut kids = vec![];
        for i in 0..n {
            let c = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 24.into()]),
                    Operation::new("Td", vec![50.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(format!("P{}", i))]),
                    Operation::new("ET", vec![]),
                ],
            };
            let cid = doc.add_object(Stream::new(dictionary! {}, c.encode().unwrap()));
            let pid = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id, "Contents" => cid,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(pid.into());
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => n as i64, "Resources" => res,
            }),
        );
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", cat);
        doc.save(path).unwrap();
    }

    fn texts(path: &str) -> Vec<String> {
        let d = Document::load(path).unwrap();
        d.get_pages().keys().map(|&n| d.extract_text(&[n]).unwrap_or_default().trim().to_string()).collect()
    }

    #[test]
    fn reorder_delete_duplicate_rotate_blank() {
        let dir = std::env::temp_dir();
        let src = dir.join("pg_src.pdf").to_string_lossy().to_string();
        let out = dir.join("pg_out.pdf").to_string_lossy().to_string();
        sample(5, &src);
        let specs = vec![
            PageSpec { src: 3, rot: 0, blank: false },
            PageSpec { src: 0, rot: 90, blank: false },
            PageSpec { src: 0, rot: -90, blank: false }, // duplicate of page 1, other direction
            PageSpec { src: 2, rot: 0, blank: true },    // new blank page
            PageSpec { src: 4, rot: 180, blank: false },
        ];
        let r = arrange(&src, &specs, &out).unwrap();
        assert_eq!(r.pages, 5);
        let d = Document::load(&out).unwrap();
        let pages = d.get_pages();
        assert_eq!(pages.len(), 5);
        assert_eq!(texts(&out), vec!["P3", "P0", "P0", "", "P4"]);
        let rots: Vec<i64> = pages
            .values()
            .map(|&id| d.get_dictionary(id).unwrap().get(b"Rotate").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0))
            .collect();
        assert_eq!(rots, vec![0, 90, 270, 0, 180]);
        // every page keeps its fonts through the inherited Resources
        for &id in pages.values() {
            assert!(d.get_dictionary(id).unwrap().has(b"Resources"));
        }
    }

    #[test]
    fn nested_page_tree_keeps_inherited_settings() {
        let dir = std::env::temp_dir();
        let src = dir.join("pg_nest.pdf").to_string_lossy().to_string();
        let out = dir.join("pg_nest_out.pdf").to_string_lossy().to_string();
        let mut doc = Document::with_version("1.5");
        let root = doc.new_object_id();
        let mid = doc.new_object_id();
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
        let mut kids = vec![];
        for i in 0..3 {
            let c = doc.add_object(Stream::new(dictionary! {}, format!("BT /F1 12 Tf 10 10 Td (N{}) Tj ET", i).into_bytes()));
            let link = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![0.into(), 0.into(), 5.into(), 5.into()], "Dest" => vec![Object::Integer(0)] });
            let uri = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![0.into(), 0.into(), 5.into(), 5.into()], "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com") } });
            let p = doc.add_object(dictionary! { "Type" => "Page", "Parent" => mid, "Contents" => c, "Annots" => vec![link.into(), uri.into()] });
            kids.push(Object::from(p));
        }
        doc.objects.insert(mid, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Parent" => root, "Kids" => kids, "Count" => 3,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 400.into()], "Rotate" => 90,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        }));
        doc.objects.insert(root, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![mid.into()], "Count" => 3 }));
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => root });
        doc.trailer.set("Root", cat);
        doc.save(&src).unwrap();

        let specs = vec![
            PageSpec { src: 2, rot: 0, blank: false },
            PageSpec { src: 0, rot: 90, blank: false },
        ];
        arrange(&src, &specs, &out).unwrap();
        let d = Document::load(&out).unwrap();
        let pages = d.get_pages();
        assert_eq!(pages.len(), 2);
        let mut rots = vec![];
        for &id in pages.values() {
            let pd = d.get_dictionary(id).unwrap();
            assert!(pd.has(b"MediaBox") && pd.has(b"Resources"));
            rots.push(pd.get(b"Rotate").unwrap().as_i64().unwrap());
            // internal link removed, web link kept
            assert_eq!(pd.get(b"Annots").unwrap().as_array().unwrap().len(), 1);
        }
        assert_eq!(rots, vec![90, 180]);
        assert_eq!(texts(&out), vec!["N2", "N0"]);
    }

    #[test]
    fn rejects_bad_input() {
        let dir = std::env::temp_dir();
        let src = dir.join("pg_src2.pdf").to_string_lossy().to_string();
        let out = dir.join("pg_out2.pdf").to_string_lossy().to_string();
        sample(2, &src);
        assert!(arrange(&src, &[], &out).is_err());
        assert!(arrange(&src, &[PageSpec { src: 9, rot: 0, blank: false }], &out).is_err());
    }
}
