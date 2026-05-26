import { drizzle } from "drizzle-orm/postgres-js";
import postgres from "postgres";
import * as schema from "./schema.js";

const url = process.env.DATABASE_URL;
if (!url) {
  console.error("DATABASE_URL is not set.");
  console.error(
    "  source: postgres://app:app@localhost:55432/appdb\n" +
      "  clone : postgres://postgres:postgres@localhost:55433/postgres",
  );
  process.exit(1);
}

// One connection keeps ordering predictable for the demo workload.
export const client = postgres(url, { max: 1, onnotice: () => {} });
export const db = drizzle(client, { schema });
export { schema };
