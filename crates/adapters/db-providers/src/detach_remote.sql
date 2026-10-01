DROP SERVER IF EXISTS gfs_remote_srv CASCADE;
UPDATE gfs.clone_source SET whole_cached = true, no_partial = true
 WHERE EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'gfs');
