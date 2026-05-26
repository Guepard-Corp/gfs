-- plpgsql audit trigger on products. On INSERT it records the full row as an
-- hstore; on UPDATE it records only the changed keys via the hstore difference
-- operator (NEW - OLD). Demonstrates plpgsql + hstore working together.
--
-- NOTE: triggers and functions are SOURCE-side logic. The GFS overlay clone
-- replicates data (copy-on-read) and extensions, but NOT triggers — so writes
-- performed against the clone will not populate audit_log there. The demo app
-- reports this difference rather than failing on it.
CREATE OR REPLACE FUNCTION audit_products() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  diff hstore;
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO audit_log (table_name, op, row_id, changes)
    VALUES ('products', 'INSERT', NEW.id, hstore(NEW) - 'created_at'::text - 'updated_at'::text);
    RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN
    diff := hstore(NEW) - hstore(OLD);
    IF diff <> ''::hstore THEN
      INSERT INTO audit_log (table_name, op, row_id, changes)
      VALUES ('products', 'UPDATE', NEW.id, diff);
    END IF;
    RETURN NEW;
  END IF;
  RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS products_audit ON products;
CREATE TRIGGER products_audit
  AFTER INSERT OR UPDATE ON products
  FOR EACH ROW EXECUTE FUNCTION audit_products();
