import { randomUUID } from "node:crypto";
import { sql } from "drizzle-orm";
import {
  bigint,
  customType,
  index,
  integer,
  pgTable,
  primaryKey,
  text,
  timestamp,
  uniqueIndex,
  uuid,
} from "drizzle-orm/pg-core";

/**
 * hstore <-> Record<string, string>.
 *
 * The driver sees hstore as an unknown type, so values arrive/leave as the text
 * literal `"k"=>"v", ...`. We serialize on the way in and parse on the way out.
 */
export const hstore = customType<{
  data: Record<string, string>;
  driverData: string;
}>({
  dataType() {
    return "hstore";
  },
  toDriver(value) {
    return Object.entries(value)
      .map(
        ([k, v]) =>
          `"${k.replace(/"/g, '\\"')}"=>"${String(v).replace(/"/g, '\\"')}"`,
      )
      .join(", ");
  },
  fromDriver(value) {
    const out: Record<string, string> = {};
    if (!value) return out;
    const re = /"((?:[^"\\]|\\.)*)"\s*=>\s*"((?:[^"\\]|\\.)*)"/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(value)) !== null) {
      out[m[1].replace(/\\"/g, '"')] = m[2].replace(/\\"/g, '"');
    }
    return out;
  },
});

// Client-generated UUIDs: the value is always sent explicitly, which keeps
// INSERTs working through the clone's INSTEAD OF overlay views (a view column
// carries no DEFAULT, so a server-side uuid_generate_v4() default would not
// fire there). The DDL still declares the default, so uuid-ossp stays required.
const clientUuid = () =>
  uuid("id")
    .primaryKey()
    .$defaultFn(() => randomUUID());

export const tenants = pgTable("tenants", {
  id: clientUuid(),
  slug: text("slug").notNull().unique(),
  name: text("name").notNull(),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

export const products = pgTable(
  "products",
  {
    id: clientUuid(),
    tenantId: uuid("tenant_id").notNull(),
    sku: text("sku").notNull(),
    name: text("name").notNull(),
    description: text("description"),
    attributes: hstore("attributes").notNull().default({}),
    priceCents: integer("price_cents").notNull().default(0),
    // Server-side timestamps (idiomatic Drizzle). The app relies on the DB
    // DEFAULT now(); a faithful clone must preserve it so this same app works
    // unchanged on the clone.
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
    updatedAt: timestamp("updated_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    uniqueIndex("products_tenant_sku_uq").on(t.tenantId, t.sku),
    index("products_name_trgm").using("gin", sql`${t.name} gin_trgm_ops`),
    index("products_attrs_gin").using("gin", t.attributes),
  ],
);

export const productTags = pgTable(
  "product_tags",
  {
    productId: uuid("product_id").notNull(),
    tag: text("tag").notNull(),
  },
  (t) => [primaryKey({ columns: [t.productId, t.tag] })],
);

export const orders = pgTable("orders", {
  id: clientUuid(),
  tenantId: uuid("tenant_id").notNull(),
  status: text("status").notNull().default("pending"),
  totalCents: integer("total_cents").notNull().default(0),
  placedAt: timestamp("placed_at", { withTimezone: true }).notNull().defaultNow(),
});

export const orderItems = pgTable(
  "order_items",
  {
    orderId: uuid("order_id").notNull(),
    productId: uuid("product_id").notNull(),
    qty: integer("qty").notNull(),
    unitPriceCents: integer("unit_price_cents").notNull(),
  },
  (t) => [primaryKey({ columns: [t.orderId, t.productId] })],
);

export const auditLog = pgTable("audit_log", {
  id: bigint("id", { mode: "number" }).primaryKey(),
  tableName: text("table_name").notNull(),
  op: text("op").notNull(),
  rowId: uuid("row_id"),
  changes: hstore("changes"),
  at: timestamp("at", { withTimezone: true }).notNull().defaultNow(),
});
