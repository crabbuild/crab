import {
  BookOpen,
  Newspaper,
  Briefcase,
  CreditCard,
} from "lucide-react"

import { NavigationMenuContent } from "@/components/ui/navigation-menu"
import { MegaMenuGroup } from "./mega-menu-products"
import type { MegaMenuGroupProps } from "./mega-menu-products"

const resourceNavItems: MegaMenuGroupProps["items"] = [
  { icon: BookOpen, name: "Documentation", description: "Guides, API reference, tutorials", href: "/docs" },
  { icon: Newspaper, name: "Blog", description: "Product updates and deep-dives", href: "/blog" },
  { icon: Briefcase, name: "Use Cases", description: "Industry-specific workflows", href: "/use-cases" },
  { icon: CreditCard, name: "Pricing", description: "No SaaS fee — cloud costs only", href: "/pricing" },
]

export function MegaMenuResources() {
  return (
    <NavigationMenuContent>
      <MegaMenuGroup items={resourceNavItems} />
    </NavigationMenuContent>
  )
}
