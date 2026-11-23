-- Registration offsets per (printer, paper, rendering path). One row is the
-- complete calibration for that target: a new set replaces the old values.
CREATE TABLE IF NOT EXISTS calibration (
    printer_name TEXT NOT NULL,
    paper_name TEXT NOT NULL,
    path_kind TEXT NOT NULL,
    dx_mm REAL NOT NULL,
    dy_mm REAL NOT NULL,
    scale REAL NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (printer_name, paper_name, path_kind)
);

CREATE INDEX IF NOT EXISTS calibration_printer ON calibration (printer_name);
