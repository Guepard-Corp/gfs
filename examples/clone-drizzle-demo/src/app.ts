// Demo workload. Runs the same checks against whatever DATABASE_URL points to,
// so it can be pointed at the source DB or at the GFS clone and the output
// compared. Core checks (1-6) must pass; diagnostics are informational and
// reflect features that legitimately differ on an overlay clone.
import { randomUUID } from "node:crypto";
import { and, eq, sql } from "drizzle-orm";
import { client, db } from "./db.js";
import { productTags, products } from "./schema.js";

let coreFailures = 0;

function ok(label: string, detail = "") {
  console.log(`  \x1b[32m✓\x1b[0m ${label}${detail ? ` — ${detail}` : ""}`);
}
function fail(label: string, err: unknown) {
  coreFailures++;
  console.log(`  \x1b[31m✗\x1b[0m ${label} — ${(err as Error).message}`);
}
function info(label: string, detail: string) {
  console.log(`  \x1b[36mi\x1b[0m ${label} — ${detail}`);
}

async function rows<T = Record<string, unknown>>(query: ReturnType<typeof sql>): Promise<T[]> {
  return (await db.execute(query)) as unknown as T[];
}

async function core(label: string, fn: () => Promise<string>) {
  try {
    ok(label, await fn());
  } catch (e) {
    fail(label, e);
  }
}

async function diag(label: string, fn: () => Promise<string>) {
  try {
    info(label, await fn());
  } catch (e) {
    info(label, `unavailable here (${(e as Error).message.split("\n")[0]})`);
  }
}

async function main() {
  console.log(`\nWorkload against ${process.env.DATABASE_URL?.replace(/:[^:@/]*@/, ":***@")}\n`);

  // 1. Data is visible (on the clone these reads are served copy-on-read).
  await core("data visible", async () => {
    const r = await rows<{ t: string; p: string; pt: string; o: string; oi: string }>(sql`
      SELECT (SELECT count(*) FROM tenants)::text AS t,
             (SELECT count(*) FROM products)::text AS p,
             (SELECT count(*) FROM product_tags)::text AS pt,
             (SELECT count(*) FROM orders)::text AS o,
             (SELECT count(*) FROM order_items)::text AS oi`);
    const { t, p, pt, o, oi } = r[0];
    return `${t} tenants, ${p} products, ${pt} tags, ${o} orders, ${oi} items`;
  });

  // 2. pg_trgm fuzzy search (note the typo 'wireles').
  await core("pg_trgm fuzzy search", async () => {
    const term = "wireles";
    const r = await rows<{ name: string; sim: number }>(sql`
      SELECT name, round(word_similarity(${term}, name)::numeric, 2) AS sim
      FROM products
      WHERE word_similarity(${term}, name) > 0.4
      ORDER BY sim DESC, name
      LIMIT 5`);
    if (r.length === 0) throw new Error("no fuzzy matches for 'wireles'");
    return `${r.length} matches, top: "${r[0].name}" (sim ${r[0].sim})`;
  });

  // 3. hstore filtering (containment + key existence).
  await core("hstore attribute filters", async () => {
    const r = await rows<{ acme: string; waterproof: string; wireless: string }>(sql`
      SELECT (SELECT count(*) FROM products WHERE attributes -> 'brand' = 'Acme')::text AS acme,
             (SELECT count(*) FROM products WHERE attributes ? 'waterproof')::text AS waterproof,
             (SELECT count(*) FROM products WHERE attributes @> 'wireless=>true'::hstore)::text AS wireless`);
    const { acme, waterproof, wireless } = r[0];
    return `brand=Acme:${acme}, has 'waterproof':${waterproof}, wireless=>true:${wireless}`;
  });

  // 4. Composite-key join (order_items PK is (order_id, product_id)).
  await core("composite-key join (order_items)", async () => {
    const r = await rows<{ name: string; qty: number }>(sql`
      SELECT p.name, oi.qty
      FROM order_items oi
      JOIN products p ON p.id = oi.product_id
      ORDER BY p.name
      LIMIT 5`);
    if (r.length === 0) throw new Error("no order items joined");
    return `${r.length} line items, e.g. ${r[0].qty}× "${r[0].name}"`;
  });

  // 5. Write through the (overlay, on the clone) tables: insert + composite-key
  //    tag + update, then read back via Drizzle.
  let newId = randomUUID();
  await core("write path (insert + composite tag + update)", async () => {
    const tenant = await rows<{ id: string }>(sql`SELECT id FROM tenants ORDER BY slug LIMIT 1`);
    const tenantId = tenant[0].id;
    const sku = `DEMO-${newId.slice(0, 8)}`;

    await db.insert(products).values({
      id: newId,
      tenantId,
      sku,
      name: "Wireless Demo Widget",
      description: "inserted by app.ts",
      attributes: { brand: "Acme", color: "red", wireless: "true" },
      priceCents: 1234,
    });
    await db.insert(productTags).values([
      { productId: newId, tag: "demo" },
      { productId: newId, tag: "wireless" },
    ]);
    await db
      .update(products)
      .set({ priceCents: 4321 })
      .where(eq(products.id, newId));

    const back = await db
      .select({ name: products.name, price: products.priceCents, attrs: products.attributes })
      .from(products)
      .where(and(eq(products.id, newId)));
    if (back.length !== 1 || back[0].price !== 4321) {
      throw new Error("read-back mismatch after write");
    }
    return `inserted+updated "${back[0].name}" (color=${back[0].attrs.color}, price=${back[0].price})`;
  });

  // 6. Read-your-write across a composite-key table.
  await core("read-your-write (tags)", async () => {
    const r = await rows<{ tag: string }>(sql`
      SELECT tag FROM product_tags WHERE product_id = ${newId} ORDER BY tag`);
    const tags = r.map((x) => x.tag).join(", ");
    if (!tags.includes("demo")) throw new Error("inserted tag not found");
    return `tags: ${tags}`;
  });

  console.log("\n  Diagnostics (expected to differ on an overlay clone):");

  // plpgsql audit trigger: fires on the SOURCE (base table), not on the clone
  // overlay (triggers are not mirrored). Reports the count rather than asserting.
  await diag("plpgsql audit trigger", async () => {
    const r = await rows<{ n: string }>(sql`
      SELECT count(*)::text AS n FROM audit_log WHERE row_id = ${newId}`);
    return `audit_log rows for this write: ${r[0].n} (2 on source; 0 on clone — triggers not mirrored)`;
  });

  // pg_stat_statements: needs shared_preload_libraries (set on the source, not
  // on the GFS-provisioned clone) to return rows.
  await diag("pg_stat_statements", async () => {
    const r = await rows<{ q: string; calls: number }>(sql`
      SELECT query AS q, calls FROM pg_stat_statements
      WHERE query ILIKE '%products%' ORDER BY calls DESC LIMIT 1`);
    if (r.length === 0) return "extension present, no tracked statements";
    return `top tracked: ${r[0].calls} calls`;
  });

  // dblink: round-trips through the extension to prove it is installed. The
  // server-side connection uses the in-container loopback (127.0.0.1:5432) with
  // credentials taken from DATABASE_URL, so it works on source and clone alike.
  await diag("dblink round-trip", async () => {
    const u = new URL(process.env.DATABASE_URL!);
    const pass = decodeURIComponent(u.password);
    const r = await rows<{ n: number }>(sql`
      SELECT * FROM dblink(
        'host=127.0.0.1 port=5432 user=' || current_user
          || ' password=' || ${pass} || ' dbname=' || current_database(),
        'SELECT count(*)::int FROM products') AS t(n int)`);
    return `dblink saw ${r[0].n} products`;
  });

  console.log();
  if (coreFailures > 0) {
    console.log(`\x1b[31mFAILED: ${coreFailures} core check(s) failed.\x1b[0m\n`);
    await client.end();
    process.exit(1);
  }
  console.log("\x1b[32mAll core checks passed.\x1b[0m\n");
  await client.end();
}

main().catch(async (e) => {
  console.error("app crashed:", e);
  await client.end();
  process.exit(1);
});
