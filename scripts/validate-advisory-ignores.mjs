import fs from "node:fs";
import process from "node:process";

const path = process.argv[2] || "deny.toml";
const source = fs.readFileSync(path, "utf8");
const advisories = source.match(/\[advisories\]([\s\S]*?)(?:\n\[[^\]]+\]|\s*$)/)?.[1];
if (!advisories) {
  throw new Error(`${path} has no [advisories] section`);
}

const ignoreBlock = advisories.match(/\bignore\s*=\s*\[([\s\S]*?)\]/)?.[1] || "";
const ignoredIds = [...ignoreBlock.matchAll(/\bid\s*=\s*"([^"]+)"/g)].map(
  (match) => match[1],
);
const reviewedEntries = [
  ...ignoreBlock.matchAll(
    /\bid\s*=\s*"([^"]+)"[\s\S]*?\breason\s*=\s*"[^"]*?Review by (\d{4}-\d{2}-\d{2})\.[^"]*"/g,
  ),
].map((match) => ({ id: match[1], reviewBy: match[2] }));

if (ignoredIds.length !== reviewedEntries.length) {
  throw new Error(
    "every ignored advisory must have a reason containing `Review by YYYY-MM-DD.`",
  );
}

const today = new Date().toISOString().slice(0, 10);
for (const { id, reviewBy } of reviewedEntries) {
  const parsed = new Date(`${reviewBy}T00:00:00Z`);
  if (Number.isNaN(parsed.valueOf()) || parsed.toISOString().slice(0, 10) !== reviewBy) {
    throw new Error(`${id} has an invalid review date: ${reviewBy}`);
  }
  if (reviewBy < today) {
    throw new Error(`${id} review expired on ${reviewBy}`);
  }
}

console.log(`validated ${reviewedEntries.length} advisory ignore review dates`);
