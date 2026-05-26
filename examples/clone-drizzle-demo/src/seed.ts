// Seeds the source database with a small but varied catalog: two tenants,
// products with hstore attributes and trigram-friendly names, composite-key
// tags, and orders. Idempotent-ish: truncates first so re-runs are clean.
import { sql } from "drizzle-orm";
import { client, db } from "./db.js";
import { orderItems, orders, productTags, products, tenants } from "./schema.js";

type ProductSeed = {
  sku: string;
  name: string;
  description: string;
  attributes: Record<string, string>;
  priceCents: number;
  tags: string[];
};

const ACME: ProductSeed[] = [
  {
    sku: "AC-WH-001",
    name: "Wireless Noise-Cancelling Headphones",
    description: "Over-ear, 30h battery",
    attributes: { brand: "Acme", color: "black", wireless: "true", waterproof: "false" },
    priceCents: 19999,
    tags: ["audio", "wireless", "featured"],
  },
  {
    sku: "AC-WS-002",
    name: "Wireless Earbuds Pro",
    description: "In-ear, ANC",
    attributes: { brand: "Acme", color: "white", wireless: "true", waterproof: "true" },
    priceCents: 12999,
    tags: ["audio", "wireless"],
  },
  {
    sku: "AC-KB-003",
    name: "Mechanical Keyboard TKL",
    description: "Hot-swap switches",
    attributes: { brand: "Acme", color: "grey", switch: "brown", wireless: "false" },
    priceCents: 8999,
    tags: ["input", "featured"],
  },
  {
    sku: "AC-MS-004",
    name: "Ergonomic Wireless Mouse",
    description: "Vertical grip",
    attributes: { brand: "Acme", color: "black", wireless: "true" },
    priceCents: 4599,
    tags: ["input", "wireless"],
  },
  {
    sku: "AC-SP-005",
    name: "Portable Bluetooth Speaker",
    description: "IP67 rugged",
    attributes: { brand: "Acme", color: "blue", wireless: "true", waterproof: "true" },
    priceCents: 7499,
    tags: ["audio", "outdoor"],
  },
];

const GLOBEX: ProductSeed[] = [
  {
    sku: "GX-MN-101",
    name: "27-inch 4K Monitor",
    description: "IPS, USB-C",
    attributes: { brand: "Globex", color: "silver", panel: "IPS", waterproof: "false" },
    priceCents: 34999,
    tags: ["display", "featured"],
  },
  {
    sku: "GX-WB-102",
    name: "1080p Webcam with Privacy Shutter",
    description: "Autofocus",
    attributes: { brand: "Globex", color: "black", wireless: "false" },
    priceCents: 6999,
    tags: ["video"],
  },
  {
    sku: "GX-DK-103",
    name: "USB-C Docking Station",
    description: "11-in-1",
    attributes: { brand: "Globex", color: "grey", ports: "11" },
    priceCents: 12999,
    tags: ["accessory", "featured"],
  },
  {
    sku: "GX-CH-104",
    name: "Ergonomic Mesh Office Chair",
    description: "Lumbar support",
    attributes: { brand: "Globex", color: "black", material: "mesh" },
    priceCents: 28999,
    tags: ["furniture"],
  },
  {
    sku: "GX-LT-105",
    name: "Wireless Desk Lamp",
    description: "Qi charging base",
    attributes: { brand: "Globex", color: "white", wireless: "true" },
    priceCents: 5499,
    tags: ["accessory", "wireless"],
  },
];

async function seedTenant(slug: string, name: string, items: ProductSeed[]) {
  const [tenant] = await db.insert(tenants).values({ slug, name }).returning();

  const insertedProducts = await db
    .insert(products)
    .values(
      items.map((p) => ({
        tenantId: tenant.id,
        sku: p.sku,
        name: p.name,
        description: p.description,
        attributes: p.attributes,
        priceCents: p.priceCents,
      })),
    )
    .returning();

  const tagRows = insertedProducts.flatMap((row, i) =>
    items[i].tags.map((tag) => ({ productId: row.id, tag })),
  );
  await db.insert(productTags).values(tagRows);

  // One order picking the first two products of the tenant.
  const picks = insertedProducts.slice(0, 2);
  const total = picks.reduce((sum, p) => sum + p.priceCents, 0);
  const [order] = await db
    .insert(orders)
    .values({ tenantId: tenant.id, status: "paid", totalCents: total })
    .returning();
  await db.insert(orderItems).values(
    picks.map((p) => ({
      orderId: order.id,
      productId: p.id,
      qty: 1,
      unitPriceCents: p.priceCents,
    })),
  );

  return insertedProducts.length;
}

async function main() {
  // Reset (CASCADE handles FKs); RESTART IDENTITY resets the audit_log sequence.
  await db.execute(
    sql`TRUNCATE tenants, products, product_tags, orders, order_items, audit_log RESTART IDENTITY CASCADE`,
  );

  const a = await seedTenant("acme", "Acme Corp", ACME);
  const g = await seedTenant("globex", "Globex Inc", GLOBEX);

  console.log(`Seeded ${a + g} products across 2 tenants.`);
  await client.end();
}

main().catch(async (e) => {
  console.error("seed failed:", e);
  await client.end();
  process.exit(1);
});
