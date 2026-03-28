//! Explicit placeholder handlers for UI affordances that do not have product logic yet.

fn log_stub(name: &str, detail: &str) {
    eprintln!("[ui-todo] {name}: {detail}");
}

// TODO: implement a server discovery / explore view.
pub(crate) fn todo_explore_servers() {
    log_stub(
        "todo_explore_servers",
        "Server discovery is not implemented yet.",
    );
}

// TODO: implement a threads surface for the active channel.
pub(crate) fn todo_open_threads() {
    log_stub(
        "todo_open_threads",
        "Channel threads are not implemented yet.",
    );
}

// TODO: implement per-channel notification settings.
pub(crate) fn todo_open_notifications() {
    log_stub(
        "todo_open_notifications",
        "Notification settings are not implemented yet.",
    );
}

// TODO: implement pinned message browsing.
pub(crate) fn todo_open_pins() {
    log_stub("todo_open_pins", "Pinned messages are not implemented yet.");
}

// TODO: implement message search for the active channel/server.
pub(crate) fn todo_search_messages() {
    log_stub("todo_search_messages", "Search is not implemented yet.");
}

// TODO: implement an inbox / mentions feed.
pub(crate) fn todo_open_inbox() {
    log_stub("todo_open_inbox", "Inbox is not implemented yet.");
}

// TODO: implement a help / support surface.
pub(crate) fn todo_open_help() {
    log_stub("todo_open_help", "Help center is not implemented yet.");
}

// TODO: implement a GIF picker and insertion flow.
pub(crate) fn todo_insert_gif() {
    log_stub("todo_insert_gif", "GIF picker is not implemented yet.");
}

// TODO: implement an emoji picker and insertion flow.
pub(crate) fn todo_open_emoji_picker() {
    log_stub(
        "todo_open_emoji_picker",
        "Emoji picker is not implemented yet.",
    );
}

// TODO: implement a sticker picker and insertion flow.
pub(crate) fn todo_open_sticker_picker() {
    log_stub(
        "todo_open_sticker_picker",
        "Sticker picker is not implemented yet.",
    );
}

// TODO: implement a member profile / context surface.
pub(crate) fn todo_open_member_profile(user_id: i64) {
    log_stub(
        "todo_open_member_profile",
        &format!("Member profile is not implemented yet for user {user_id}."),
    );
}
