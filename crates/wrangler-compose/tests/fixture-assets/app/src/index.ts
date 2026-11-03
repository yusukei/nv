export default {
  async fetch(req: Request, env: any): Promise<Response> {
    const u = new URL(req.url);
    if (u.pathname === "/api/hello") { return new Response("api"); }
    if (u.pathname === "/api/file") { return env.ASSETS.fetch(new URL("/assets/app.js", u)); }
    if (u.pathname === "/api/page") { return env.ASSETS.fetch(new URL("/p/42", u)); }
    return new Response(`app worker ${u.pathname}`);
  },
};
