// Locator resolver JS — the page-side half of `text=` / `role=` resolution.
//
//
// We mark the matched element with a transient attribute and `querySelector` it
// (rather than returning coords) so the existing nodeId-based click/focus paths
// are reused unchanged. The marker is stripped after resolution. Resolution
// happens at record time only; replay is structural, so the marker never enters
// the hash chain.

use loom_shared::locator::{parse_locator, Segment};

pub(super) const MARKER_ATTR: &str = "data-loom-loc";

pub(super) const MARKER_SELECTOR: &str = "[data-loom-loc]";

/// Shared JS: clear any stale marker, plus `vis()` (visible: non-zero box and no
/// display:none/visibility:hidden) and `norm()` (collapse whitespace + trim).
const JS_PRELUDE: &str = "var M='data-loom-loc';document.querySelectorAll('['+M+']').forEach(function(e){e.removeAttribute(M);});function vis(e){var r=e.getBoundingClientRect();if(r.width===0&&r.height===0)return false;var s=getComputedStyle(e);return s.display!=='none'&&s.visibility!=='hidden';}function norm(t){return (t||'').replace(/\\s+/g,' ').trim();}";

/// `<input type=…>` → the implicit ARIA role `role=` matches it by (a subset of
/// Playwright's table; a type absent here has no role). The date/time family is
/// a `textbox` — Playwright's role for them — so `role=textbox[name="First day"]`
/// finds a native `<input type="date">`. number/range/file/image deliberately
/// stay role-less: giving file/image `button` would let a styled-away upload
/// input win a `role=button` selector (the resolver keeps the shortest name).
const INPUT_TYPE_ROLES: &[(&str, &str)] = &[
    ("text", "textbox"),
    ("email", "textbox"),
    ("password", "textbox"),
    ("search", "textbox"),
    ("tel", "textbox"),
    ("url", "textbox"),
    ("date", "textbox"),
    ("datetime-local", "textbox"),
    ("month", "textbox"),
    ("time", "textbox"),
    ("week", "textbox"),
    ("color", "textbox"),
    ("checkbox", "checkbox"),
    ("radio", "radio"),
    ("button", "button"),
    ("submit", "button"),
    ("reset", "button"),
];

/// W3C-AccName subset: implicit role mapping (`roleOf`, its `<input>` roles from
/// [`INPUT_TYPE_ROLES`]) + accessible-name computation (`accName`: aria-label →
/// aria-labelledby → associated label/placeholder → text → title).
fn role_helpers_js() -> &'static str {
    static JS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    JS.get_or_init(|| {
        let input_roles: serde_json::Map<String, serde_json::Value> = INPUT_TYPE_ROLES
            .iter()
            .map(|(ty, role)| ((*ty).to_string(), serde_json::Value::from(*role)))
            .collect();
        let input_roles = serde_json::Value::Object(input_roles).to_string();
        format!("var INPUT_ROLES={input_roles};{ROLE_OF_JS}{ACC_NAME_JS}")
    })
}

const ROLE_OF_JS: &str = "function roleOf(e){var r=e.getAttribute('role');if(r)return r.trim().toLowerCase();var tag=e.tagName.toLowerCase();if(tag==='button')return 'button';if(tag==='a'&&e.hasAttribute('href'))return 'link';if(tag==='select')return 'combobox';if(tag==='textarea')return 'textbox';if(/^h[1-6]$/.test(tag))return 'heading';if(tag==='input'){var ty=(e.getAttribute('type')||'text').toLowerCase();return Object.prototype.hasOwnProperty.call(INPUT_ROLES,ty)?INPUT_ROLES[ty]:'';}return '';}";

const ACC_NAME_JS: &str = "function accName(e){var al=e.getAttribute('aria-label');if(al&&al.trim())return norm(al);var lb=e.getAttribute('aria-labelledby');if(lb){var txt=lb.split(/\\s+/).map(function(id){var t=document.getElementById(id);return t?t.textContent:'';}).join(' ');if(norm(txt))return norm(txt);}var tag=e.tagName.toLowerCase();if(tag==='input'||tag==='textarea'||tag==='select'){if(e.id){try{var lbl=document.querySelector('label[for=\"'+(window.CSS&&CSS.escape?CSS.escape(e.id):e.id)+'\"]');if(lbl&&norm(lbl.textContent))return norm(lbl.textContent);}catch(_e){}}var pl=e.getAttribute('placeholder');if(pl&&pl.trim())return pl.trim();}var tc=norm(e.textContent);if(tc)return tc;var ti=e.getAttribute('title');if(ti&&ti.trim())return ti.trim();return '';}";

fn wrap(body: &str) -> String {
    let mut s = String::from("(function(){");
    s.push_str(body);
    s.push_str("})()");
    s
}

/// A JS EXPRESSION that evaluates to the element `locator` names, or `null` —
/// the host-side grammar and matching (`css=` / `text=` / `role=`, `frame=`
/// descending into a same-origin frame's document, ` >> ` composition) for verbs
/// that resolve their target page-side (the guest `web.type mode:"value"`,
/// `web.select`, `web.hover`, `web.scroll`). `text=`/`role=` reuse the very
/// resolver bodies the host path runs, scoped to the current frame's document by
/// shadowing `document`, and like the host path they must be the LAST segment.
/// `None` when the locator does not parse.
pub fn locator_element_js(locator: &str) -> Option<String> {
    let segments = parse_locator(locator).ok()?;
    let last = segments.len().checked_sub(1)?;
    let mut js = String::from("(function(){var doc=document,scope=doc;");
    for (i, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Frame(css) => {
                let css = serde_json::to_string(css).ok()?;
                js.push_str(&format!(
                    "var f=scope.querySelector({css});if(!f||!f.contentDocument)return null;doc=f.contentDocument;scope=doc;"
                ));
            }
            Segment::Css(css) => {
                let css = serde_json::to_string(css).ok()?;
                js.push_str(&format!(
                    "scope=scope.querySelector({css});if(!scope)return null;"
                ));
            }
            Segment::Text(_) | Segment::Role(_) => {
                if i != last {
                    return Some("null".to_string());
                }
                let iife = marker_resolver_js(seg)?;
                let body = iife.strip_prefix("(function(){")?.strip_suffix("})()")?;
                js.push_str(&format!(
                    "if(!(function(document){{{body}}})(doc))return null;\
                     var hit=doc.querySelector('{MARKER_SELECTOR}');\
                     if(hit)hit.removeAttribute('{MARKER_ATTR}');return hit;"
                ));
            }
        }
    }
    js.push_str("return scope===doc?null:scope;})()");
    Some(js)
}

/// JS resolver expression for a `text=`/`role=` segment, or `None` for others.
pub(super) fn marker_resolver_js(seg: &Segment) -> Option<String> {
    match seg {
        Segment::Text(needle) => Some(text_resolver_js(needle)),
        Segment::Role(spec) => {
            let (role, name) = parse_role_spec(spec);
            Some(role_resolver_js(&role, name.as_deref()))
        }
        Segment::Css(_) | Segment::Frame(_) => None,
    }
}

/// Deepest *visible* element whose normalized text contains `needle` (case-
/// insensitive). Candidates are the visible, text-matching elements; an element is
/// disqualified when it *contains another candidate* — i.e. the real match is
/// nested deeper — so a full-width wrapper never wins over the tight control it
/// contains. Disqualification is visibility-gated on purpose: a hidden child (a
/// `display:none`/`visibility:hidden` twin, a `.sr-only` label, an inline
/// `<script>`/`<style>` whose code happens to contain the text) is not a
/// candidate, so it neither steals the click nor makes its visible parent
/// unresolvable. Among the remaining deepest candidates we prefer the shortest
/// normalized text (closest to an exact match), then the smallest bounding-box
/// area. This is Playwright `getByText()` semantics. (The old code ranked all
/// matches by `textContent` length, which tied a wrapper with its sole-text child;
/// pre-order + strict `<` then made the wrapper win, so `web.click` landed on its
/// empty center.)
fn text_resolver_js(needle: &str) -> String {
    let n = serde_json::to_string(needle).unwrap_or_else(|_| "\"\"".into());
    let mut body = String::from(JS_PRELUDE);
    body.push_str("var needle=norm(");
    body.push_str(&n);
    body.push_str(").toLowerCase();if(!needle)return false;");
    body.push_str("var all=document.querySelectorAll('body *'),cand=[];for(var i=0;i<all.length;i++){var e=all[i];if(vis(e)&&norm(e.textContent).toLowerCase().indexOf(needle)!==-1)cand.push(e);}var best=null,bestLen=Infinity,bestArea=Infinity;for(var i=0;i<cand.length;i++){var e=cand[i],inner=false;for(var j=0;j<cand.length;j++){if(j!==i&&e.contains(cand[j])){inner=true;break;}}if(inner)continue;var len=norm(e.textContent).length,r=e.getBoundingClientRect(),area=r.width*r.height;if(len<bestLen||(len===bestLen&&area<bestArea)){best=e;bestLen=len;bestArea=area;}}if(best){best.setAttribute(M,'1');return true;}return false;");
    wrap(&body)
}

/// First *visible* element whose computed ARIA role equals `role` and (when
/// `name` is given) whose accessible name contains it (case-insensitive).
fn role_resolver_js(role: &str, name: Option<&str>) -> String {
    let role_j = serde_json::to_string(&role.to_lowercase()).unwrap_or_else(|_| "\"\"".into());
    let name_j = match name {
        Some(n) => serde_json::to_string(&n.to_lowercase()).unwrap_or_else(|_| "\"\"".into()),
        None => "null".into(),
    };
    let mut body = String::from(JS_PRELUDE);
    body.push_str(role_helpers_js());
    body.push_str("var wantRole=");
    body.push_str(&role_j);
    body.push_str(";var wantName=");
    body.push_str(&name_j);
    body.push_str(";var best=null,bestLen=1e9,all=document.querySelectorAll('body *');for(var i=0;i<all.length;i++){var e=all[i];if(!vis(e))continue;if(roleOf(e)!==wantRole)continue;var an=norm(accName(e)).toLowerCase();if(wantName!==null&&an.indexOf(wantName)===-1)continue;if(an.length<bestLen){best=e;bestLen=an.length;}}if(best){best.setAttribute(M,'1');return true;}return false;");
    wrap(&body)
}

/// Split `role=` value `NAME[name="X"]` into `("name"…, Some("X"))`. Tolerates
/// single/double quotes and an unquoted value terminated by `]`/space.
fn parse_role_spec(spec: &str) -> (String, Option<String>) {
    match spec.find('[') {
        Some(br) => {
            let role = spec[..br].trim().to_string();
            let rest = &spec[br..];
            let name = rest.find("name=").map(|i| &rest[i + 5..]).map(|s| {
                let s = s.trim_start();
                let (quote, s) = match s.chars().next() {
                    Some(q @ ('"' | '\'')) => (Some(q), &s[1..]),
                    _ => (None, s),
                };
                let end = match quote {
                    Some(q) => s.find(q).unwrap_or(s.len()),
                    None => s.find([']', ' ']).unwrap_or(s.len()),
                };
                s[..end].to_string()
            });
            (role, name)
        }
        None => (spec.trim().to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_role_spec_extracts_role_and_quoted_name() {
        assert_eq!(
            parse_role_spec(r#"button[name="Send"]"#),
            ("button".into(), Some("Send".into()))
        );
        assert_eq!(parse_role_spec("button"), ("button".into(), None));
        assert_eq!(
            parse_role_spec("textbox[name='Email']"),
            ("textbox".into(), Some("Email".into()))
        );
    }

    #[test]
    fn text_resolver_embeds_needle_safely() {
        // A needle with a quote must not break out of the JS string literal.
        let js = text_resolver_js(r#"a"b"#);
        assert!(
            js.contains(r#""a\"b""#),
            "needle must be JSON-escaped: {js}"
        );
        assert!(js.starts_with("(function(){") && js.ends_with("})()"));
    }

    #[test]
    fn text_resolver_disqualifies_wrapper_via_nested_visible_candidate() {
        let js = text_resolver_js("Continue");
        // Candidacy is visibility-gated: a hidden child must neither be clicked nor
        // disqualify its visible parent.
        assert!(
            js.contains("vis(e)"),
            "candidates must be visibility-gated: {js}"
        );
        // A wrapper is disqualified when it CONTAINS another (visible) candidate, so
        // the click lands on the tight control, not the wrapper's empty center.
        assert!(
            js.contains("e.contains("),
            "must disqualify ancestors that contain a nested visible match: {js}"
        );
        // Among deepest candidates, prefer the closest-to-exact text then the
        // tightest box — not a global smallest-area pick.
        assert!(
            js.contains("bestLen")
                && js.contains("bestArea")
                && js.contains("getBoundingClientRect"),
            "must rank by shortest text then bounding-box area: {js}"
        );
    }

    #[test]
    fn role_resolver_includes_accname_helpers() {
        let js = role_resolver_js("button", Some("Continue"));
        assert!(js.contains("function accName"));
        assert!(
            js.contains("\"continue\""),
            "name lowercased + embedded: {js}"
        );
    }

    #[test]
    fn locator_element_js_descends_frames_and_scopes_css() {
        let js = locator_element_js("frame=#w >> css=#composer").unwrap();
        assert!(js.contains(r##"scope.querySelector("#w")"##), "{js}");
        assert!(js.contains("doc=f.contentDocument"), "{js}");
        assert!(js.contains(r##"scope.querySelector("#composer")"##), "{js}");
        assert!(js.ends_with("return scope===doc?null:scope;})()"), "{js}");
    }

    #[test]
    fn locator_element_js_runs_the_host_resolver_against_the_current_document() {
        let js = locator_element_js(r#"role=textbox[name="First day"]"#).unwrap();
        let host =
            marker_resolver_js(&Segment::Role(r#"textbox[name="First day"]"#.into())).unwrap();
        let body = host
            .strip_prefix("(function(){")
            .and_then(|b| b.strip_suffix("})()"))
            .unwrap();
        assert!(
            js.contains(&format!("(function(document){{{body}}})(doc)")),
            "{js}"
        );
        assert!(js.contains("hit.removeAttribute('data-loom-loc')"), "{js}");
    }

    #[test]
    fn locator_element_js_mirrors_the_host_limits() {
        // text=/role= only as the last segment (as on the host path).
        assert_eq!(
            locator_element_js("text=Go >> css=#b").as_deref(),
            Some("null")
        );
        // A selector with a quote cannot break out of the JS string.
        let js = locator_element_js(r#"css=input[name="q"]"#).unwrap();
        assert!(
            js.contains(r#"scope.querySelector("input[name=\"q\"]")"#),
            "{js}"
        );
    }

    fn input_role(input_type: &str) -> Option<&'static str> {
        INPUT_TYPE_ROLES
            .iter()
            .find(|(ty, _)| *ty == input_type)
            .map(|(_, role)| *role)
    }

    #[test]
    fn date_family_inputs_are_textboxes() {
        for ty in ["date", "datetime-local", "month", "time", "week", "color"] {
            assert_eq!(input_role(ty), Some("textbox"), "{ty}");
        }
    }

    #[test]
    fn input_roles_are_otherwise_unchanged() {
        for ty in ["text", "email", "password", "search", "tel", "url"] {
            assert_eq!(input_role(ty), Some("textbox"), "{ty}");
        }
        assert_eq!(input_role("checkbox"), Some("checkbox"));
        assert_eq!(input_role("radio"), Some("radio"));
        for ty in ["button", "submit", "reset"] {
            assert_eq!(input_role(ty), Some("button"), "{ty}");
        }
        // Deliberately role-less: a file/image input as `button` could win a
        // `role=button` selector over the visible control.
        for ty in ["number", "range", "file", "image", "hidden"] {
            assert_eq!(input_role(ty), None, "{ty}");
        }
    }

    #[test]
    fn role_helpers_embed_the_whole_input_role_table() {
        let js = role_helpers_js();
        let start = js.find("var INPUT_ROLES=").expect("table declared") + "var INPUT_ROLES=".len();
        let end = start + js[start..].find(';').expect("table terminated");
        let table: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&js[start..end]).expect("table is JSON");
        assert_eq!(table.len(), INPUT_TYPE_ROLES.len());
        for (ty, role) in INPUT_TYPE_ROLES {
            assert_eq!(table.get(*ty).and_then(|r| r.as_str()), Some(*role), "{ty}");
        }
        assert!(js.contains("function roleOf") && js.contains("function accName"));
    }
}
