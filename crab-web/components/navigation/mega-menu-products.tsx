import Link from "next/link"
import { Terminal, HardDrive, ShieldCheck } from "lucide-react"
import type { LucideIcon } from "lucide-react"

import {
  NavigationMenuContent,
  NavigationMenuLink,
} from "@/components/ui/navigation-menu"

export interface MegaMenuGroupProps {
  items: Array<{
    icon: LucideIcon
    name: string
    description: string
    href: string
  }>
}

export function MegaMenuGroup({ items }: MegaMenuGroupProps) {
  return (
    <ul className="grid w-[400px] gap-1 md:w-[500px] md:grid-cols-2 lg:w-[600px]">
      {items.map((item) => (
        <li key={item.href}>
          <NavigationMenuLink
            render={<Link href={item.href} />}
            className="flex items-start gap-3 rounded-md p-3 text-sm transition-colors hover:bg-muted focus:bg-muted"
          >
            <div className="rounded-md bg-muted p-2">
              <item.icon className="size-4 text-foreground" />
            </div>
            <div className="space-y-1">
              <p className="text-sm leading-none font-medium">{item.name}</p>
              <p className="line-clamp-2 text-sm text-muted-foreground">
                {item.description}
              </p>
            </div>
          </NavigationMenuLink>
        </li>
      ))}
    </ul>
  )
}

const productNavItems: MegaMenuGroupProps["items"] = [
  {
    icon: Terminal,
    name: "CLI",
    description: "Git remote helper for cloud storage",
    href: "/cli",
  },
  {
    icon: ShieldCheck,
    name: "Crab Auth Service",
    description: "Cloud-native authentication",
    href: "/auth",
  },
  {
    icon: HardDrive,
    name: "Crab Cache Service",
    description: "Local and remote caching layer",
    href: "/cache",
  },
]

export function MegaMenuProducts() {
  return (
    <NavigationMenuContent>
      <MegaMenuGroup items={productNavItems} />
    </NavigationMenuContent>
  )
}
