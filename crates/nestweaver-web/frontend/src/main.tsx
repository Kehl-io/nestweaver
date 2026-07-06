import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Routes, Route } from "react-router-dom";
import "@fontsource-variable/inter/index.css";
import "@fontsource/michroma/index.css";
import "@fontsource-variable/jetbrains-mono/index.css";
import "./index.css";
import App from "./App";
import { AdminLogin } from "./pages/admin/AdminLogin";
import { AdminLayout } from "./pages/admin/AdminLayout";
import { Overview } from "./pages/admin/Overview";
import { Repos } from "./pages/admin/Repos";
import { Queue } from "./pages/admin/Queue";
import { DeadLetter } from "./pages/admin/DeadLetter";
import { Settings } from "./pages/admin/Settings";
import { DeviceApprove } from "./pages/admin/DeviceApprove";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <Routes>
        <Route path="/admin/login" element={<AdminLogin />} />
        <Route path="/admin" element={<AdminLayout />}>
          <Route index element={<Overview />} />
          <Route path="repos" element={<Repos />} />
          <Route path="queue" element={<Queue />} />
          <Route path="dead-letter" element={<DeadLetter />} />
          <Route path="settings" element={<Settings />} />
          <Route path="device-approve" element={<DeviceApprove />} />
        </Route>
        <Route path="*" element={<App />} />
      </Routes>
    </BrowserRouter>
  </StrictMode>
);
