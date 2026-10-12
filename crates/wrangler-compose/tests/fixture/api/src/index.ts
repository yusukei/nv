import { DurableObject } from "cloudflare:workers";
export class Counter extends DurableObject {
  async inc(): Promise<number> { const n = ((await this.ctx.storage.get<number>("n")) ?? 0) + 1; await this.ctx.storage.put("n", n); return n; }
}
export default {
  async fetch(req: Request, env: any): Promise<Response> {
    const u = new URL(req.url);
    if (u.pathname === "/write") { await env.DB.prepare("INSERT INTO items(src) VALUES (?)").bind(u.searchParams.get("src") ?? "api").run(); return new Response("ok"); }
    if (u.pathname === "/upload") { await env.UPLOADS.put("up/" + (u.searchParams.get("k") ?? "x"), "data"); return new Response("uploaded"); }
    if (u.pathname === "/remove") { await env.UPLOADS.delete("up/" + (u.searchParams.get("k") ?? "x")); return new Response("removed"); }
    if (u.pathname === "/seed") {
      await env.DB.batch([
        env.DB.prepare("INSERT INTO authors(id, name) VALUES (1, 'Ann')"),
        env.DB.prepare("INSERT INTO posts(author, title) VALUES (1, 'Hello')"),
        env.DB.prepare("INSERT INTO acf_fields(k) VALUES ('x')"),
      ]);
      await env.FILES.put("meta.txt", "m", { customMetadata: { title: "café 日本" }, httpMetadata: { contentType: "text/plain; charset=utf-8" } });
      await env.FILES.put("d/../e", "dot");
      await env.CACHE.put("ü", "v", { metadata: { note: "日本" } });
      await env.DB.batch([
        env.DB.prepare("INSERT INTO big(rowid, n, x, t) VALUES (42, 9007199254740993, 1.0, char(1) || 'z')"),
        env.DB.prepare("INSERT INTO big(rowid, n, x, t) VALUES (43, 0, x'00ff10', 'blob')"),
        env.DB.prepare("INSERT INTO seqd(v) VALUES ('a'), ('b'), ('c')"),
        env.DB.prepare("DELETE FROM seqd WHERE id = 3"),
      ]);
      console.log("first line\nsecond line");
      return new Response("seeded");
    }
    if (u.pathname === "/blog") {
      const r = await env.DB.prepare("SELECT a.name, p.slug, (SELECT count(*) FROM acf_fields) AS c FROM posts p JOIN authors a ON a.id = p.author").first();
      const m = await env.FILES.head("meta.txt");
      const d = await env.FILES.get("d/../e");
      const e = await env.FILES.get("e");
      const k = await env.CACHE.getWithMetadata("ü");
      return Response.json({ ...r, title: m?.customMetadata?.title, dot: d ? await d.text() : null, e: e ? "present" : null, kv: k.metadata });
    }
    if (u.pathname === "/values") {
      const r = await env.DB.prepare("SELECT CAST(n AS TEXT) AS n, typeof(x) AS x, rowid AS id, t = char(1) || 'z' AS t FROM big").first();
      return Response.json(r);
    }
    if (u.pathname === "/blob") {
      const r = await env.DB.prepare("SELECT hex(x) AS h, typeof(x) AS t FROM big WHERE t = 'blob'").first();
      return Response.json(r);
    }
    if (u.pathname === "/next") {
      const r = await env.DB.prepare("INSERT INTO seqd(v) VALUES ('d') RETURNING id").first();
      return Response.json(r);
    }
    if (u.pathname === "/secret") { return new Response(env.STRIPE_KEY); }
    if (u.pathname === "/cron") { return new Response((await env.CACHE.get("cron")) ?? "none"); }
    if (u.pathname === "/count") { const r = await env.DB.prepare("SELECT count(*) n FROM items").first(); return Response.json(r); }
    await env.FILES.put("a.txt", "from-api");
    await env.CACHE.put("k", "v");
    await env.JOBS.send({ at: Date.now() });
    const n = await env.COUNTER.get(env.COUNTER.idFromName("x")).inc();
    console.log("api handled", u.pathname);
    return new Response(`api ${env.GREETING} do=${n} secret=${env.STRIPE_KEY ? "yes" : "no"}`);
  },
  async scheduled(controller: ScheduledController, env: any) { console.log("cron", controller.cron); await env.CACHE.put("cron", controller.cron); },
  async queue(batch: MessageBatch, env: any) { for (const m of batch.messages) console.log("msg", JSON.stringify(m.body)); await env.CACHE.put("consumed", String(batch.messages.length)); },
};
