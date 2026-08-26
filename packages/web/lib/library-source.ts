import { library } from "collections/server"
import { loader } from "fumadocs-core/source"

export const librarySource = loader({
  baseUrl: "/library",
  source: library.toFumadocsSource(),
})
