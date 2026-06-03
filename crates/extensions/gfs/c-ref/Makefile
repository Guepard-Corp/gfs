# PGXS build for the gfs PoC. Builds against the postgres pointed to by
# pg_config (override: make PG_CONFIG=/path/to/pg_config).
MODULE_big = gfs
OBJS = gfs.o
EXTENSION = gfs
DATA = gfs--0.0.1.sql
PGFILEDESC = "gfs - GFS lazy-clone Table Access Method (PoC)"

PG_CONFIG ?= pg_config
PGXS := $(shell $(PG_CONFIG) --pgxs)
include $(PGXS)
