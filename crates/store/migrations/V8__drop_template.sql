-- S2 drops the removed template feature. Historical V4/V6 stay untouched so
-- an upgraded database keeps its user_version sequence; Store::open writes a
-- backup before running this migration.
DROP TABLE IF EXISTS template;
