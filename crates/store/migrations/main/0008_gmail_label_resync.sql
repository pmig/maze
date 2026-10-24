-- Maze store, schema v8: re-read Gmail labels once.
--
-- Earlier builds didn't unescape Gmail's quoted system labels (`"\\Inbox"`, `"\\Sent"`,
-- `"\\Starred"`, `"\\Important"`), so mail synced from All Mail never joined the Inbox,
-- Sent, Starred and Important label folders. Setting All Mail's HIGHESTMODSEQ back to 1
-- makes the next pass fetch every message's flags and labels again
-- (`UID FETCH 1:* (FLAGS X-GM-LABELS) (CHANGEDSINCE 1)`) and fill those folders. Gmail
-- has no QRESYNC, so the pass takes the CONDSTORE path. Headers and bodies are not
-- fetched again.
UPDATE folder
SET highestmodseq = 1
WHERE role = 'all'
  AND is_label = 0
  AND highestmodseq IS NOT NULL
  AND account_id IN (SELECT id FROM account WHERE kind = 'gmail');
