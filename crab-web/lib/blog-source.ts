import { blog } from 'collections/server';
import { loader } from 'fumadocs-core/source';

export const blogSource = loader({
  baseUrl: '/blog',
  source: blog.toFumadocsSource(),
});
