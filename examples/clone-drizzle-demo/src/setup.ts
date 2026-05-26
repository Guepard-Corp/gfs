// Applies the raw SQL files (extensions, schema, triggers) to DATABASE_URL.
// Each file is sent as a single simple-protocol query so plpgsql `$$` bodies
// and multi-statement files run exactly as psql would execute them.
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { client } from "./db.js";

const here = dirname(fileURLToPath(import.meta.url));
const sqlDir = join(here, "..", "sql");

const files = ["00-extensions.sql", "01-schema.sql", "02-triggers.sql"];

async function main() {
  for (const f of files) {
    const text = await readFile(join(sqlDir, f), "utf8");
    process.stdout.write(`  applying ${f} ... `);
    await client.unsafe(text);
    console.log("ok");
  }
  console.log("Schema ready.");
  await client.end();
}

main().catch(async (e) => {
  console.error("\nsetup failed:", e);
  await client.end();
  process.exit(1);
});
