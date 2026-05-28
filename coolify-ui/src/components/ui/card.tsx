import { cn } from "@/lib/utils";
import type { HTMLAttributes, PropsWithChildren } from "react";
export function Card({ className, ...props }: PropsWithChildren<HTMLAttributes<HTMLDivElement>>) { return <div className={cn("rounded-lg border bg-card text-card-foreground shadow-sm", className)} {...props} />; }
export function CardHeader({ className, ...props }: PropsWithChildren<HTMLAttributes<HTMLDivElement>>) { return <div className={cn("flex flex-col space-y-1.5 p-6", className)} {...props} />; }
export function CardTitle({ className, ...props }: PropsWithChildren<HTMLAttributes<HTMLHeadingElement>>) { return <h3 className={cn("text-lg font-semibold leading-none tracking-tight", className)} {...props} />; }
export function CardContent({ className, ...props }: PropsWithChildren<HTMLAttributes<HTMLDivElement>>) { return <div className={cn("p-6 pt-0", className)} {...props} />; }
