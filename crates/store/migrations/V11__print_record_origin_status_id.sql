-- `records.list` filters origin + status and then orders by id. The V10
-- index has printer between status and id, so SQLite needed a temporary sort.
-- This index keeps both filters and the id order.
CREATE INDEX IF NOT EXISTS print_record_origin_status_id
    ON print_record (origin, status, id);
