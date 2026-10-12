# Lessons

- The capture task reads `CaptureRequest`s only after `InputCapture::new` returns; while a portal dialog waits, anything that expects a reply (e.g. `Focus`) hangs and fires late. Gate such requests in the service on `capture_status`/`capture_awaiting` (fixed for `cli focus` in 10fffe9).
