import { defineConfig } from "drizzle-kit";

// Provided for convenience (e.g. `drizzle-kit studio` / `pull`). The demo uses
// the raw SQL files in ./sql as the source of truth so that extensions and the
// plpgsql trigger are created deterministically; src/schema.ts mirrors them as
// the typed query layer.
export default defineConfig({
  dialect: "postgresql",
  schema: "./src/schema.ts",
  dbCredentials: {
    url: process.env.DATABASE_URL ?? "postgres://app:app@localhost:55432/appdb",
  },
});
