-- Registration calibration was removed from the product. Historical V3/V7
-- stay untouched so an upgraded database keeps its user_version sequence;
-- this migration drops the data so no stale rows remain.
DROP TABLE IF EXISTS calibration;
