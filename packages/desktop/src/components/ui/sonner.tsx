import { Toaster as Sonner } from "sonner";

type ToasterProps = React.ComponentProps<typeof Sonner>;

const Toaster = ({ theme = "system", ...props }: ToasterProps) => {
  return (
    <Sonner
      theme={theme}
      className="toaster group"
      style={
        {
          "--normal-bg": "rgb(var(--popover))",
          "--normal-text": "rgb(var(--popover-foreground))",
          "--normal-border": "rgb(var(--border))",
        } as React.CSSProperties
      }
      toastOptions={{
        classNames: {
          // Sonner injects unlayered CSS after the packaged app stylesheet.
          // Token overrides must win that order, including high-contrast themes.
          description: "!text-muted-foreground",
          actionButton:
            "!bg-primary !text-primary-foreground",
          cancelButton:
            "!bg-muted !text-muted-foreground",
        },
      }}
      {...props}
    />
  );
};

export { Toaster };
