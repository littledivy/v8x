import React from "npm:react@18.3.1";
import { renderToString } from "npm:react-dom@18.3.1/server";
console.log(renderToString(React.createElement("h1", null, "ssr")));
