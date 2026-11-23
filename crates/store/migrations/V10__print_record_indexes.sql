-- Indexes for the records list/summary/stats filters and their id ordering.
-- `julianday(at)` is an expression index because the date filters compare
-- `julianday(at)` rather than raw text, which would not use a plain `at` index.
DROP INDEX IF EXISTS print_record_origin;
CREATE INDEX IF NOT EXISTS print_record_origin_status_printer_id
    ON print_record (origin, status, printer, id);
CREATE INDEX IF NOT EXISTS print_record_status_printer_id
    ON print_record (status, printer, id);
CREATE INDEX IF NOT EXISTS print_record_printer_id
    ON print_record (printer, id);
CREATE INDEX IF NOT EXISTS print_record_at_julianday
    ON print_record (julianday(at));
