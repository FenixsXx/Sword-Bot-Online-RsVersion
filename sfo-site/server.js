const http = require("node:http");
const fs = require("node:fs/promises");
const path = require("node:path");
const crypto = require("node:crypto");

const ROOT = __dirname;
const DATA_DIR = path.join(ROOT, "data");
const DATA_FILE = path.join(DATA_DIR, "reports.json");
const PORT = Number(process.env.PORT || 7070);
const PASSWORD = process.env.SFO_PASSWORD || "kirito237";
const sessions = new Set();

const seedReports = [
  { name: "Serata Minecraft", category: "Minigiochi", date: "2026-10-18", time: "21:00", attendees: 24, newMembers: 8, names: "Alex, Giulia, Sam, Nina, Teo, Vale, Leo, Mia", duration: 120, responsible: "Marco Rossi", notes: "", clipRecorded: true, clipRegistrar: "Marco Rossi", flop: false },
  { name: "Chiacchiere in voice", category: "Musica", date: "2026-10-15", time: "21:30", attendees: 17, newMembers: 3, names: "Luca, Fede, Ary", duration: 90, responsible: "Giulia Bianchi", notes: "", clipRecorded: true, clipRegistrar: "Marco Rossi", flop: false },
  { name: "Torneo Rocket League", category: "Quiz", date: "2026-10-12", time: "20:45", attendees: 31, newMembers: 6, names: "Chris, Bea, Dani, Max, Joe, Eli", duration: 150, responsible: "Marco Rossi", notes: "", clipRecorded: true, clipRegistrar: "Giulia Bianchi", flop: false },
];

async function ensureData() {
  await fs.mkdir(DATA_DIR, { recursive: true });
  try { await fs.access(DATA_FILE); } catch { await fs.writeFile(DATA_FILE, `${JSON.stringify(seedReports, null, 2)}\n`); }
}
async function readReports() {
  const reports = JSON.parse(await fs.readFile(DATA_FILE, "utf8"));
  const categoryMap = { "Gaming night": "Minigiochi", Community: "Film", "Voice hangout": "Musica" };
  return reports.map((report) => ({
    ...report,
    id: report.id || crypto.createHash("sha256").update(JSON.stringify(report)).digest("hex").slice(0, 24),
    category: categoryMap[report.category] || report.category,
    clipRecorded: report.clipRecorded ?? report.registered ?? false,
    clipRegistrar: report.clipRegistrar || report.registrar || "",
    flop: report.flop ?? false,
  }));
}
async function writeReports(reports) { await fs.writeFile(DATA_FILE, `${JSON.stringify(reports, null, 2)}\n`); }
function parseCookies(request) { return Object.fromEntries((request.headers.cookie || "").split(";").filter(Boolean).map((item) => item.trim().split("="))); }
function authenticated(request) { return sessions.has(parseCookies(request).sfo_session); }
function send(response, status, body, headers = {}) { response.writeHead(status, { "Content-Type": "application/json; charset=utf-8", ...headers }); response.end(JSON.stringify(body)); }
async function body(request) { let value = ""; for await (const chunk of request) value += chunk; return JSON.parse(value || "{}"); }
function safePath(urlPath) { const file = path.normalize(path.join(ROOT, urlPath === "/" ? "index.html" : urlPath)); return file.startsWith(ROOT) ? file : null; }

const server = http.createServer(async (request, response) => {
  try {
    const url = new URL(request.url, `http://${request.headers.host}`);
    if (url.pathname === "/api/login" && request.method === "POST") {
      const input = await body(request);
      if (input.password !== PASSWORD) return send(response, 401, { error: "Password non valida." });
      const token = crypto.randomBytes(24).toString("hex");
      sessions.add(token);
      return send(response, 200, { ok: true }, { "Set-Cookie": `sfo_session=${token}; HttpOnly; SameSite=Lax; Path=/` });
    }
    if (url.pathname === "/api/logout" && request.method === "POST") {
      sessions.delete(parseCookies(request).sfo_session);
      return send(response, 204, null, { "Set-Cookie": "sfo_session=; Max-Age=0; HttpOnly; SameSite=Lax; Path=/" });
    }
    if (url.pathname === "/api/session" && request.method === "GET") return authenticated(request) ? send(response, 200, { ok: true }) : send(response, 401, { error: "Non autenticato." });
    if ((url.pathname === "/api/reports" || url.pathname.startsWith("/api/reports/")) && !authenticated(request)) return send(response, 401, { error: "Non autenticato." });
    if (url.pathname === "/api/reports" && request.method === "GET") return send(response, 200, await readReports());
    const deletePath = url.pathname.match(/^\/api\/reports\/([^/]+)\/?$/);
    if (deletePath && (request.method === "DELETE" || request.method === "POST")) {
      const id = decodeURIComponent(deletePath[1]);
      const reports = await readReports();
      const remaining = reports.filter((report) => report.id !== id);
      if (remaining.length === reports.length) return send(response, 404, { error: "Report non trovato." });
      await writeReports(remaining);
      return send(response, 200, { ok: true });
    }
    if (url.pathname === "/api/reports" && request.method === "POST") {
      const report = await body(request);
      const required = ["name", "category", "date", "time", "duration", "attendees", "newMembers", "responsible"];
      const categories = ["Minigiochi", "Anime", "Film", "Quiz", "Disegno", "Musica"];
      if (required.some((key) => report[key] === undefined || report[key] === "")) return send(response, 400, { error: "Compila tutti i campi obbligatori." });
      if (!categories.includes(report.category)) return send(response, 400, { error: "Categoria non valida." });
      if (![15, 30, 45, 60, 75, 90, 105, 120, 150, 180, 240].includes(Number(report.duration))) return send(response, 400, { error: "La durata deve essere a intervalli di 15 minuti." });
      if (Number(report.newMembers) > Number(report.attendees)) return send(response, 400, { error: "I nuovi membri non possono superare le presenze." });
      if (report.clipRecorded && !String(report.clipRegistrar || "").trim()) return send(response, 400, { error: "Indica chi ha registrato la clip." });
      const reports = await readReports();
      reports.unshift({ ...report, duration: Number(report.duration), attendees: Number(report.attendees), newMembers: Number(report.newMembers), id: crypto.randomUUID(), createdAt: new Date().toISOString() });
      await writeReports(reports);
      return send(response, 201, reports[0]);
    }
    if (request.method !== "GET") return send(response, 404, { error: "Risorsa non trovata." });
    const file = safePath(url.pathname);
    if (url.pathname.startsWith("/data/")) return send(response, 403, { error: "Accesso negato." });
    if (!file) return send(response, 403, { error: "Accesso negato." });
    const content = await fs.readFile(file);
    const types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".json": "application/json; charset=utf-8", ".svg": "image/svg+xml", ".ico": "image/x-icon" };
    response.writeHead(200, { "Content-Type": types[path.extname(file)] || "application/octet-stream" });
    response.end(content);
  } catch (error) {
    console.error(error);
    send(response, 500, { error: "Errore interno del server." });
  }
});

ensureData().then(() => server.listen(PORT, () => console.log(`SFO Staff Hub: http://localhost:${PORT}`)));
