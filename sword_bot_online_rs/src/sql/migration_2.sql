
CREATE TABLE IF NOT EXISTS suggestion_review_messages (
    suggestion_id INTEGER PRIMARY KEY REFERENCES suggestions(id) ON DELETE CASCADE,
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS daily_vote_activity (
    user_id INTEGER NOT NULL,
    poll_id INTEGER NOT NULL,
    day TEXT NOT NULL,
    PRIMARY KEY (user_id, poll_id, day)
);
