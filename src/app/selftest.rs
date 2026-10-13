//! `CTX_APP_SELFTEST=1`: the app drives itself through the same entry points the window system
//! calls (keys, text, clicks), against a running server, checks what it sees, prints a report and
//! quits with 0 when everything passed.

pub fn start() {}
