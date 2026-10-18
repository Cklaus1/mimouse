//! The tools `tools/list` announces: names, descriptions, input schemas and
//! annotations.

use serde_json::{Value, json};

/// Read by the model before it uses the tools (`initialize` result).
pub(super) const INSTRUCTIONS: &str = "MiMouse drives the desktop of this machine (target \"self\") \
or of a paired peer: screenshots, pointer, clicks, keys, typing, scrolling and the clipboard. \
Start with list_targets to see each target's capabilities, grants and state. \
Coordinates for pointer_to, click and screenshot regions are pixels of the last screenshot of \
that target (a zoomed region screenshot or an observed frame counts as the last one): take a \
screenshot first and read positions off it. Pass `observe: true` to an action to get the screen \
after it settles in the same call. A person using the target ends the session: the tool reports \
`preempted`; then stop and ask the user before doing anything else on that target. \
Errors `denied`, `busy` and `unavailable` also need the user: report them instead of retrying.";

const TARGET: &str = "Machine to act on: \"self\" (default) or a target from list_targets \
(client id or host name).";

const COORDS: &str = "\"image\" (default): x/y are pixels of the last screenshot of this target. \
\"desktop\": x/y are full-resolution desktop pixels (origin at the top-left of all monitors).";

const OBSERVE: &str = "After the action, wait until the screen stops changing and return a \
screenshot in the same call (it becomes the last screenshot for coordinates). true for the \
defaults, or {settle_ms, timeout_ms}.";

const PREEMPT: &str = " If it fails with `preempted`, a person is using the target: stop and ask \
the user.";

fn target() -> Value {
    json!({"type": "string", "description": TARGET})
}

fn coords() -> Value {
    json!({"type": "string", "enum": ["image", "desktop"], "default": "image", "description": COORDS})
}

fn observe() -> Value {
    json!({
        "description": OBSERVE,
        "anyOf": [
            {"type": "boolean"},
            {
                "type": "object",
                "properties": {
                    "settle_ms": {"type": "integer", "minimum": 0,
                        "description": "how long the screen must stay unchanged (default 300)"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": proto_max_timeout(),
                        "description": "longest wait for the screen to settle (default 3000)"}
                },
                "additionalProperties": false
            }
        ]
    })
}

fn proto_max_timeout() -> u32 {
    crate::agent::proto::Observe::MAX_TIMEOUT_MS
}

/// `readOnly`: the tool changes nothing on the target; `destructive`: it
/// may change or delete data there (clients ask before running such tools)
fn annotations(title: &str, read_only: bool, destructive: bool) -> Value {
    let mut a = json!({"title": title, "readOnlyHint": read_only, "openWorldHint": false});
    if !read_only {
        a["destructiveHint"] = json!(destructive);
    }
    a
}

fn tool(
    name: &str,
    description: String,
    properties: Value,
    required: &[&str],
    annotations: Value,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
        "annotations": annotations,
    })
}

/// every tool, in the order `tools/list` returns them
pub(super) fn list(max_edge: u32) -> Vec<Value> {
    vec![
        tool(
            "list_targets",
            "The machines this agent can act on, each with its OS, capabilities (screen, absolute \
             pointer, text input, clipboard), the grants it gives this agent and its state (locked, \
             capturing, idle time, session held). Call this first."
                .into(),
            json!({}),
            &[],
            annotations("List targets", true, false),
        ),
        tool(
            "session_open",
            format!(
                "Opens a control session on the target, which shows a notice naming this agent and \
                 `reason`. Actions open one implicitly; open it explicitly to state the reason.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "reason": {"type": "string", "description": "shown to the person at the target"}
            }),
            &[],
            annotations("Open session", false, false),
        ),
        tool(
            "session_close",
            "Ends the session on the target and releases every key and button it holds. Close it \
             when the task is done."
                .into(),
            json!({"target": target()}),
            &[],
            annotations("Close session", false, false),
        ),
        tool(
            "screenshot",
            format!(
                "Captures the target's screen as an image, scaled down so its longest edge is at \
                 most max_edge (default {max_edge}). The text part gives the image size, the scale \
                 (desktop pixels per image pixel) and the desktop position of its top-left corner. \
                 Coordinates you pass to pointer_to and click are pixels of this image. To read small \
                 text, take a screenshot of a region: its image then becomes the reference for \
                 coordinates."
            ),
            json!({
                "target": target(),
                "region": {
                    "type": "object",
                    "description": "part of the screen to capture, in `coords` units",
                    "properties": {
                        "x": {"type": "number"}, "y": {"type": "number"},
                        "width": {"type": "number", "exclusiveMinimum": 0},
                        "height": {"type": "number", "exclusiveMinimum": 0}
                    },
                    "required": ["x", "y", "width", "height"]
                },
                "coords": coords(),
                "max_edge": {"type": "integer", "minimum": 1,
                    "description": "longest edge of the returned image in pixels"},
                "format": {"type": "string", "enum": ["png", "jpeg"], "default": "png"}
            }),
            &[],
            annotations("Screenshot", true, false),
        ),
        tool(
            "pointer_to",
            format!(
                "Moves the pointer to x,y: pixels of the last screenshot of this target unless \
                 coords is \"desktop\". Take a screenshot first.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "x": {"type": "number"},
                "y": {"type": "number"},
                "coords": coords(),
                "observe": observe()
            }),
            &["x", "y"],
            annotations("Move pointer", false, false),
        ),
        tool(
            "click",
            format!(
                "Clicks a mouse button (count 2 = double click). With x,y it first moves the pointer \
                 there (pixels of the last screenshot unless coords is \"desktop\"); without, it \
                 clicks where the pointer is.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"},
                "count": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1},
                "x": {"type": "number"},
                "y": {"type": "number"},
                "coords": coords(),
                "observe": observe()
            }),
            &[],
            annotations("Click", false, true),
        ),
        tool(
            "key",
            format!(
                "Presses a key chord and releases it: key names joined by `+`, e.g. \"ctrl+l\", \
                 \"shift+tab\", \"enter\", \"super\", \"f5\". For text use the `type` tool.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "keys": {"type": "string", "description": "chord such as \"ctrl+l\""},
                "observe": observe()
            }),
            &["keys"],
            annotations("Press keys", false, true),
        ),
        tool(
            "type",
            format!(
                "Types text into the focused field in the target's keyboard layout. Characters the \
                 layout cannot produce are skipped and reported as unmapped.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "text": {"type": "string"},
                "observe": observe()
            }),
            &["text"],
            annotations("Type text", false, true),
        ),
        tool(
            "scroll",
            format!(
                "Scrolls by wheel notches at the pointer position: positive dy scrolls down, \
                 positive dx right. Move the pointer over the content first.{PREEMPT}"
            ),
            json!({
                "target": target(),
                "dx": {"type": "integer", "default": 0},
                "dy": {"type": "integer", "default": 0},
                "observe": observe()
            }),
            &[],
            annotations("Scroll", false, false),
        ),
        tool(
            "clip_get",
            "Reads the target's clipboard in `mime` (default: its text, else its first format). \
             Text comes back as text, images as an image, other formats as a base64 resource; the \
             first text part lists the formats on offer."
                .into(),
            json!({
                "target": target(),
                "mime": {"type": "string"}
            }),
            &[],
            annotations("Read clipboard", true, false),
        ),
        tool(
            "clip_set",
            "Replaces the target's clipboard: pass `text`, or `mime` and `data_base64`. What the \
             clipboard held before is lost."
                .into(),
            json!({
                "target": target(),
                "text": {"type": "string"},
                "mime": {"type": "string"},
                "data_base64": {"type": "string", "description": "the contents, base64-encoded"}
            }),
            &[],
            annotations("Write clipboard", false, true),
        ),
    ]
}
