import "./app.css";
import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider, createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { queryClient } from "@/lib/queryClient";
import { AppShell } from "@/components/layout/AppShell";
import { Dashboard } from "@/routes/index";
import { ServersPage } from "@/routes/servers";
import { ServerDetailPage } from "@/routes/server-detail";
import { ClustersPage } from "@/routes/clusters";
import { EventsPage } from "@/routes/events";

const rootRoute = createRootRoute({ component: AppShell });
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: Dashboard });
const serversRoute = createRoute({ getParentRoute: () => rootRoute, path: "/servers", component: ServersPage });
const serverDetailRoute = createRoute({ getParentRoute: () => rootRoute, path: "/servers/$serverId", component: () => <ServerDetailPage serverId={serverDetailRoute.useParams().serverId} /> });
const clustersRoute = createRoute({ getParentRoute: () => rootRoute, path: "/clusters", component: ClustersPage });
const eventsRoute = createRoute({ getParentRoute: () => rootRoute, path: "/events", component: EventsPage });
const routeTree = rootRoute.addChildren([indexRoute, serversRoute, serverDetailRoute, clustersRoute, eventsRoute]);
const router = createRouter({ routeTree, defaultPreload: "intent", basepath: import.meta.env.BASE_URL.replace(/\/$/, "") || undefined });
declare module "@tanstack/react-router" { interface Register { router: typeof router } }

ReactDOM.createRoot(document.getElementById("root")!).render(<React.StrictMode><QueryClientProvider client={queryClient}><RouterProvider router={router} /></QueryClientProvider></React.StrictMode>);
