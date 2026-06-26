import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Routes, Route } from "react-router-dom";
import "./index.css";
import App from "./App";
import { AdminLogin } from "./pages/admin/AdminLogin";
import { AdminLayout } from "./pages/admin/AdminLayout";
import { Overview } from "./pages/admin/Overview";
import { Repos } from "./pages/admin/Repos";
import { Queue } from "./pages/admin/Queue";
import { DeadLetter } from "./pages/admin/DeadLetter";
import { Settings } from "./pages/admin/Settings";

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
        </Route>
        <Route path="*" element={<App />} />
      </Routes>
    </BrowserRouter>
  </StrictMode>
);
