import axios from "npm:axios@1.7.7";
Deno.serve((_req) => new Response(`${typeof axios}`));
