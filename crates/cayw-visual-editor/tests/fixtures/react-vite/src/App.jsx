import SiteHeader from './components/SiteHeader.jsx'
import HeroSection from './components/HeroSection.tsx'
import FeatureGrid from './components/FeatureGrid.jsx'
import SecondaryNote from './components/SecondaryNote.jsx'
import CounterForm from './components/CounterForm.jsx'
import DynamicPanel from './components/DynamicPanel.jsx'
import SiteFooter from './components/SiteFooter.jsx'

export default function App() {
  return (
    <div className="app-shell" data-testid="app-root">
      <SiteHeader />
      <main className="page-container" data-testid="page-container">
        <HeroSection />
        <FeatureGrid />
        <SecondaryNote />
        <CounterForm />
        <DynamicPanel />
      </main>
      <SiteFooter />
    </div>
  )
}
