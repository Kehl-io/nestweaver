import React from 'react';
import { useAuth } from './hooks/useAuth';
import { UserProfile } from './components/UserProfile';
import { Sidebar } from './components/Sidebar';

interface AppProps {
  title: string;
}

const Header = ({ title }: { title: string }) => {
  return <div className="header">{title}</div>;
};

export function App({ title }: AppProps) {
  const auth = useAuth();

  return (
    <div className="app">
      <Header title={title} />
      <Sidebar />
      <UserProfile user={auth.user} />
    </div>
  );
}

export const Dashboard = () => {
  return (
    <div>
      <App title="Dashboard" />
      <span>Footer</span>
    </div>
  );
};
