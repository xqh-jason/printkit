-- Calibrations are scoped to the origin that set them. An empty origin is the
-- global/administrator scope. The triple is now a quadruple.
CREATE TABLE calibration_scoped (
    origin TEXT NOT NULL DEFAULT '',
    printer_name TEXT NOT NULL,
    paper_name TEXT NOT NULL,
    path_kind TEXT NOT NULL,
    dx_mm REAL NOT NULL,
    dy_mm REAL NOT NULL,
    scale REAL NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (origin, printer_name, paper_name, path_kind)
);
INSERT INTO calibration_scoped
    (origin, printer_name, paper_name, path_kind, dx_mm, dy_mm, scale, updated_at)
SELECT '', printer_name, paper_name, path_kind, dx_mm, dy_mm, scale, updated_at
FROM calibration;
DROP TABLE calibration;
ALTER TABLE calibration_scoped RENAME TO calibration;
CREATE INDEX calibration_printer ON calibration (printer_name);
