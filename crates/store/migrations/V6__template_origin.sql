-- Templates are scoped to the browser origin that saved them. An empty origin
-- is the administrator/global scope and remains visible everywhere.
ALTER TABLE template ADD COLUMN origin TEXT NOT NULL DEFAULT '';
